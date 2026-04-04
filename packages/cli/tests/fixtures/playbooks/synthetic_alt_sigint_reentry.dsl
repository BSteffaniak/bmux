@name synthetic-alt-sigint-reentry
@timeout 45000
@viewport cols=120 rows=30
@shell sh

new-session
send-keys keys="for i in 1 2 3 4 5 6; do echo PRE_PROMPT_$i; done\r"
wait-for pattern='PRE_PROMPT_6'
send-keys keys="state=1; trap 'state=0; printf \"\e[?10\"; sleep 0.02; printf \"49l\"' INT; printf '\e[12;34H'; printf '\e[?1049h\e[2J\e[HSEQ_TUI'; (sleep 0.6; kill -INT $$) & while [ $state -eq 1 ]; do sleep 1; done; sleep 6\r"
sleep ms=900
screen
assert-cursor row=11 col=33
