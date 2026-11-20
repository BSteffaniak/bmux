# BMUX connections administration

Local administration shared by the CLI host and the connections plugin: kiosk
profiles and access tokens (`kiosk init/status/issue-token/revoke-token/
ssh-print-config` and token-authorized attach), and remote target inventory,
initialization, probing and session completion. It owns no transport; callers
supply the probe used to reach a target.
