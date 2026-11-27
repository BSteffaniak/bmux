@timeout 300000
@shell sh

new-session
send-keys keys='seq 1 200\r'
wait-for pattern='200' timeout=60000
prefix-key key='['
send-keys keys='k\r'
