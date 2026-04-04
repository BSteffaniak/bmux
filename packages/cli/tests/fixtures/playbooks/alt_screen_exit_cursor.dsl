@timeout 20000
@viewport cols=80 rows=24
@shell sh

new-session
send-keys keys='printf "\e[2J\e[H"; printf "main"; printf "\e[12;34H"; printf "\e[?1049h"; printf "\e[4;7H"; printf "ALT_ONLY"; printf "\e[?1049l"; printf "\e[12;34H"; printf "ALT_EXIT_READY"; printf "\e[12;34H"; cat\r'
wait-for pattern='ALT_EXIT_READY'
assert-screen contains='ALT_EXIT_READY'
assert-screen not_contains='ALT_ONLY'
assert-cursor row=11 col=33
