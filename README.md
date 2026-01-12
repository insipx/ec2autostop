a mostly vibecoded aws lambda to turn off my instances after 10min of inactivity

all ec2 instances tagged with `AutoStop=true` will stop after configured
inactivity

config via envrionment variables:

- `METRIC_THRESHOLD`: Threshold for CPU to qualify as "inactive". default 5.0%
- `INACTIVITY_MINUTES`: Minutes of inactivity, default 30min
- `SNS_TOPIC_ARN`: Topic arn to send mail notif
- `MIN_DATAPOINTS`: Minimum datapoints to use to qualify for inactivity (6
  taken, default 5/6)
