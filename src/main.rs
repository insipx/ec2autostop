use std::env;
use std::fmt::Write;

use aws_config::BehaviorVersion;
use aws_sdk_cloudwatch::primitives::DateTime;
use aws_sdk_cloudwatch::types::{
    Datapoint,
    Dimension,
    Statistic,
};
use aws_sdk_ec2::types::Filter;
use aws_sdk_sns::Client as SnsClient;
use jiff::{
    Span,
    Timestamp,
};
use lambda_runtime::{
    Error,
    LambdaEvent,
    run,
    service_fn,
};
use serde::{
    Deserialize,
    Serialize,
};

#[macro_use]
extern crate tracing;

/// Minimum number of datapoints required to make a stop decision
const MIN_DATAPOINTS: u16 = 5;

/// Amount of inactivity to qualify for shutdown
const INACTIVITY_MINUTES: u16 = 30;
/// CPU % that qualifies an instance as inactive
const METRIC_THRESHOLD: f64 = 5.0;
/// Ratio of datapoints that must be below threshold to stop an instance (5/6)
const THRESHOLD_RATIO: f64 = 5.0 / 6.0;

#[derive(Deserialize)]
struct Request {}

#[derive(Serialize)]
struct Response {
    status: String,
    checked_instances: usize,
    stopped_instances: Vec<String>,
}

/// Information about a stopped instance
#[derive(Clone)]
struct StoppedInstance {
    id: String,
    name: String,
    average_cpu: f64,
    datapoint_count: usize,
}

/// Configuration loaded from environment variables
struct Config {
    tag_key: String,
    tag_value: String,
    metric_threshold: f64,
    inactivity_minutes: i64,
    sns_topic_arn: Option<String>,
    min_datapoints: u16,
}

impl Config {
    /// Loads configuration from environment variables with sensible defaults
    fn from_env() -> Self {
        Self {
            tag_key: env::var("TAG_KEY").unwrap_or_else(|_| "AutoStop".to_string()),
            tag_value: env::var("TAG_VALUE").unwrap_or_else(|_| "true".to_string()),
            metric_threshold: env::var("METRIC_THRESHOLD")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(METRIC_THRESHOLD),
            inactivity_minutes: env::var("INACTIVITY_MINUTES")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(INACTIVITY_MINUTES.into()),
            sns_topic_arn: env::var("SNS_TOPIC_ARN").ok(),
            min_datapoints: env::var("MIN_DATAPOINTS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(MIN_DATAPOINTS),
        }
    }
}

/// Checks if at least 5/6 of the datapoints are below the given threshold.
///
/// Returns `true` if the instance should be stopped based on inactivity.
/// Requires at least 5 datapoints to make a decision.
fn should_stop_instance(datapoints: &[f64], min_dps: u16, threshold: f64) -> bool {
    if datapoints.len() < min_dps.into() {
        return false;
    }

    let below_threshold_count = datapoints.iter().filter(|&&value| value < threshold).count();

    let required_below_threshold = (datapoints.len() as f64 * THRESHOLD_RATIO).ceil() as usize;
    below_threshold_count >= required_below_threshold
}

/// Retrieves the Name tag value for an EC2 instance
async fn get_instance_name(
    ec2_client: &aws_sdk_ec2::Client,
    instance_id: &str,
) -> Result<String, Error> {
    let instances = ec2_client.describe_instances().instance_ids(instance_id).send().await?;

    Ok(instances
        .reservations()
        .first()
        .and_then(|r| r.instances().first())
        .and_then(|i| i.tags().iter().find(|t| t.key() == Some("Name")).and_then(|t| t.value()))
        .unwrap_or("Unknown")
        .to_string())
}

/// Sends an SNS notification with a formatted list of stopped instances
async fn send_notification(
    sns_client: &SnsClient,
    topic_arn: &str,
    stopped_instances: &[StoppedInstance],
    config: &Config,
) -> Result<(), Error> {
    if stopped_instances.is_empty() {
        return Ok(());
    }

    let subject = format!(
        "EC2 Auto-Stop: {} instance{} stopped",
        stopped_instances.len(),
        if stopped_instances.len() == 1 { "" } else { "s" }
    );

    let mut msg = String::new();
    write!(
        msg,
        "EC2 Auto-Stop Lambda has stopped {} instance{} due to low CPU utilization.\n\n",
        stopped_instances.len(),
        if stopped_instances.len() == 1 { "" } else { "s" }
    )?;

    write!(msg, "STOPPED INSTANCES \n")?;
    write!(msg, "=================\n\n")?;

    for instance in stopped_instances {
        write!(
            msg,
            "Instance ID: {}\n\
             Instance Name: {}\n\
             Average CPU: {:.2}%\n\
             Datapoints Collected: {}\n\n",
            instance.id, instance.name, instance.average_cpu, instance.datapoint_count
        )?;
    }

    write!(msg, "\nCRITERIA\n")?;
    write!(msg, "========\n")?;
    write!(msg, "Tag Filter: {}={}\n", config.tag_key, config.tag_value)?;
    write!(msg, "CPU Threshold: {:.1}%\n", config.metric_threshold)?;
    write!(msg, "Monitoring Period: {} minutes\n", config.inactivity_minutes)?;
    write!(msg, "Threshold Rule: At least 5/6 of datapoints below threshold\n")?;

    write!(msg, "\nTimestamp: {}\n", Timestamp::now().strftime("%Y-%m-%d %H:%M:%S UTC"))?;

    sns_client.publish().topic_arn(topic_arn).subject(subject).message(msg).send().await?;

    info!("SNS notification sent to {}", topic_arn);

    Ok(())
}

async fn lambda(_event: LambdaEvent<Request>) -> Result<Response, Error> {
    let aws_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let ec2_client = aws_sdk_ec2::Client::new(&aws_config);
    let cw_client = aws_sdk_cloudwatch::Client::new(&aws_config);
    let sns_client = SnsClient::new(&aws_config);

    let config = Config::from_env();

    // Find all instances with the tag
    let instances_response = ec2_client
        .describe_instances()
        .filters(
            Filter::builder()
                .name(format!("tag:{}", config.tag_key))
                .values(&config.tag_value)
                .build(),
        )
        .filters(Filter::builder().name("instance-state-name").values("running").build())
        .send()
        .await?;

    let instances_to_check: Vec<String> = instances_response
        .reservations()
        .iter()
        .flat_map(|r| r.instances())
        .filter_map(|i| i.instance_id())
        .map(String::from)
        .collect();

    info!(
        "Found {} running instances with tag {}={}",
        instances_to_check.len(),
        config.tag_key,
        config.tag_value
    );

    let mut stopped_instances: Vec<StoppedInstance> = Vec::new();

    // Check CPU for each instance
    for instance_id in &instances_to_check {
        let end_time = Timestamp::now();
        let start_time = end_time.checked_sub(Span::new().minutes(config.inactivity_minutes))?;

        let metrics = cw_client
            .get_metric_statistics()
            .namespace("AWS/EC2")
            .metric_name("CPUUtilization")
            .dimensions(Dimension::builder().name("InstanceId").value(instance_id).build())
            .start_time(DateTime::from_secs(start_time.as_second()))
            .end_time(DateTime::from_secs(end_time.as_second()))
            .period((config.inactivity_minutes * 60 / 5) as i32) // 5-6 datapoints over the period
            .statistics(Statistic::Average)
            .send()
            .await?;

        let datapoints = metrics.datapoints();
        if !datapoints.is_empty() {
            let cpu_values: Vec<f64> =
                datapoints.iter().filter_map(|dp: &Datapoint| dp.average()).collect();

            if should_stop_instance(&cpu_values, config.min_datapoints, config.metric_threshold) {
                let below_threshold_count =
                    cpu_values.iter().filter(|&&avg| avg < config.metric_threshold).count();

                let average_cpu = cpu_values.iter().sum::<f64>() / cpu_values.len() as f64;

                info!(
                    "Instance {}: {}/{} datapoints below {:.2}% threshold - stopping",
                    instance_id,
                    below_threshold_count,
                    cpu_values.len(),
                    config.metric_threshold
                );

                let instance_name = get_instance_name(&ec2_client, instance_id).await?;

                ec2_client.stop_instances().instance_ids(instance_id).send().await?;

                stopped_instances.push(StoppedInstance {
                    id: instance_id.clone(),
                    name: instance_name.clone(),
                    average_cpu,
                    datapoint_count: cpu_values.len(),
                });
                info!("Stopped instance {} ({})", instance_id, instance_name);
            } else if cpu_values.len() < config.min_datapoints.into() {
                info!(
                    "Insufficient metrics for {} (only {} datapoints, need at least {}) - skipping",
                    instance_id,
                    cpu_values.len(),
                    config.min_datapoints
                );
            } else {
                let below_threshold_count =
                    cpu_values.iter().filter(|&&avg| avg < config.metric_threshold).count();
                info!(
                    "Instance {}: {}/{} datapoints below {:.2}% threshold - not stopping",
                    instance_id,
                    below_threshold_count,
                    cpu_values.len(),
                    config.metric_threshold
                );
            }
        }
    }

    // Send SNS notification if configured and instances were stopped
    if let Some(topic_arn) = &config.sns_topic_arn {
        if !stopped_instances.is_empty() {
            send_notification(&sns_client, topic_arn, &stopped_instances, &config).await?;
        }
    }

    Ok(Response {
        status: "complete".to_string(),
        checked_instances: instances_to_check.len(),
        stopped_instances: stopped_instances.iter().map(|i| i.id.clone()).collect(),
    })
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .without_time()
        .init();

    run(service_fn(lambda)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_stop_with_all_below_threshold() {
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 1.5, 2.5];
        assert!(should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_stop_with_exactly_five_sixths_below() {
        // 6 datapoints: need 5 below threshold
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 2.5, 10.0];
        assert!(should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_not_stop_with_less_than_five_sixths_below() {
        // 6 datapoints: only 3 below threshold (need 5)
        let datapoints = vec![1.0, 2.0, 3.0, 10.0, 12.0, 15.0];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_not_stop_with_insufficient_datapoints() {
        // Less than 5 datapoints
        let datapoints = vec![1.0, 2.0, 3.0, 4.0];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_stop_with_minimum_datapoints() {
        // 5 datapoints: need ceil(5 * 5/6) = 5 below threshold
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 1.5];
        assert!(should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_not_stop_with_minimum_datapoints_one_above() {
        // 5 datapoints: only 4 below threshold (need 5)
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 10.0];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_stop_with_seven_datapoints() {
        // 7 datapoints: need ceil(7 * 5/6) = 6 below threshold
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 1.5, 2.5, 3.5];
        assert!(should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_not_stop_with_seven_datapoints_insufficient() {
        // 7 datapoints: only 5 below threshold (need 6)
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 1.5, 10.0, 12.0];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_should_stop_with_twelve_datapoints() {
        // 12 datapoints: need ceil(12 * 5/6) = 10 below threshold
        let datapoints = vec![1.0, 2.0, 3.0, 4.0, 1.5, 2.5, 3.5, 4.5, 2.2, 3.3, 10.0, 11.0];
        assert!(should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_boundary_case_exact_threshold() {
        // Values exactly at threshold should not count as "below"
        let datapoints = vec![5.0, 5.0, 5.0, 5.0, 5.0, 1.0];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_empty_datapoints() {
        let datapoints: Vec<f64> = vec![];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }

    #[test]
    fn test_high_cpu_usage() {
        // All datapoints above threshold
        let datapoints = vec![80.0, 85.0, 90.0, 75.0, 82.0, 88.0];
        assert!(!should_stop_instance(&datapoints, MIN_DATAPOINTS, 5.0));
    }
}
