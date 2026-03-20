# Ping-pong latency test (AWS peering)

This folder contains a script that measures **both directions** of ICMP latency between two EC2 instances connected via a VPC peering connection:

- **A → B**: ping from the machine you run the script on
- **B → A**: ping initiated on the peer machine via SSH, back to this machine

It writes a timestamped **CSV** (all raw ping lines + parsed RTT) and a **summary** with p50/p95/p99.

## Prereqs

- **Security groups / NACLs** allow ICMP echo request/reply between the two instances (both directions)
- **SSH connectivity** from A → B (TCP/22 by default) using private IPs across the peering connection
- `ping` installed on both instances

## Run

From instance **A**:

```bash
cd /home/ec2-user/hummingbirdv2/test/ping_pong
python3 ping_pong_latency.py \
  --local-target-ip 10.1.2.34 \
  --remote-target-ip 10.9.8.7 \
  --ssh-host 10.1.2.34 \
  --ssh-user ec2-user \
  --ssh-key /home/ec2-user/.ssh/your-key.pem \
  --duration 60 \
  --interval 0.2 \
  --out-dir ./out
```

Notes:

- `--local-target-ip`: **B's** private IP (what A pings)
- `--remote-target-ip`: **A's** private IP (what B pings)
- `--ssh-host`: where to SSH (usually **B's** private IP)

## Output

In `--out-dir`:

- `ping_pong_YYYYMMDDTHHMMSSZ.csv`
- `ping_pong_YYYYMMDDTHHMMSSZ_summary.txt`

