# BMUX connections administration

Local administration shared by the CLI host and the connections plugin: kiosk
profiles and access tokens (`kiosk init/status/issue-token/revoke-token/
ssh-print-config` and token-authorized attach). It owns no transport: remote
hosts are paired and reached through the connections plugin.
