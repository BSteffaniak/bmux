@timeout 30000
@viewport cols=30 rows=12
@shell sh
@env-mode clean

new-session
send-keys keys='printf "\\e[2J\\e[H"; printf "abcdefghijklmnopqrstuvwxyz0123456789\\n"\r'
wait-for pattern='23456789'
resize-viewport cols=22 rows=12
screen
assert-screen matches='abcdefghijklmnopqrst\nuvwxyz0123456789'
resize-viewport cols=42 rows=12
screen
assert-screen contains='abcdefghijklmnopqrstuvwxyz0123456789'
send-keys keys='printf "\\e[2J\\e[H"; printf "hard-newline-one\\nhard-newline-two\\n"\r'
wait-for pattern='hard-newline-two'
resize-viewport cols=42 rows=12
screen
assert-screen matches='hard-newline-one\nhard-newline-two'
send-keys keys='printf "\\e[2J\\e[H"; printf "\\e[31mstyledabcdefghijklmnopqrstuvwxyz0123456789\\e[0m\\n"\r'
wait-for pattern='89'
resize-viewport cols=22 rows=12
screen
assert-screen matches='styledabcdefghijklmn\nopqrstuvwxyz01234567\n89'
