@name Presenter live demo
@viewport cols=80 rows=24
@timeout 15000
new-session
send-keys keys="printf 'LIVE BMUX DEMO\\n'\r"
wait-for pattern="LIVE BMUX DEMO"
send-keys keys="printf 'Retained final view\\n'\r"
wait-for pattern="Retained final view"
