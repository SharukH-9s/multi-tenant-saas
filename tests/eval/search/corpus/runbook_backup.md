# Backup and restore runbook

Full database backups run every night at 02:00 UTC. Transaction logs are archived continuously, which allows point-in-time recovery to any moment within the last 14 days.

## Restore drill

A restore drill is performed every quarter. The drill restores the latest backup into an isolated environment and compares row counts with production.

## Who can restore

Only members of the platform team can start a production restore, and a second engineer must approve it.
