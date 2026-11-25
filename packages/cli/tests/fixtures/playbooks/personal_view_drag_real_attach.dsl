@driver real-attach
@timeout 120000
@viewport cols=120 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/personal_view_drag_real_attach.toml
new-session name='bootstrap'
send-attach key='alt+a'
wait-rendered contains='<active:alpha>' timeout=10000
send-attach key='alt+b'
wait-rendered contains='<active:beta>' timeout=10000
send-attach key='alt+c'
wait-rendered contains='<active:gamma>' timeout=10000
# Work on a copy of this view, named work, so the default view shows what
# the drag leaves alone.
send-attach key='alt+v'
wait-rendered contains='Tab Views' timeout=10000
send-attach key='f'
send-attach key='r'
send-attach key='o'
send-attach key='m'
wait-rendered contains='New view from this one' timeout=10000
send-attach key='enter'
wait-rendered contains='New Tab View' timeout=10000
send-attach key='w'
send-attach key='o'
send-attach key='r'
send-attach key='k'
send-attach key='enter'
wait-rendered contains='View: work' timeout=10000
assert-rendered matches='<idle:alpha>[^<]*<idle:beta>[^<]*<active:gamma>'
# Drag gamma onto the left edge of alpha in the tab bar, as a terminal
# reports a press, motion with the button held, and a release.
locate id='alpha' text='<idle:alpha>'
locate id='gamma' text='<active:gamma>'
terminal-event kind=mouse phase=down button=left col='${gamma.center_col}' row='${gamma.row}'
terminal-event kind=mouse phase=drag button=left col='${alpha.end_col}' row='${alpha.row}'
terminal-event kind=mouse phase=drag button=left col='${alpha.start_col}' row='${alpha.row}'
terminal-event kind=mouse phase=up button=left col='${alpha.start_col}' row='${alpha.row}'
# The workspace label precedes the first tab of the view.
wait-rendered contains='default │  <active:gamma>' timeout=10000
assert-rendered matches='<active:gamma>[^<]*<idle:alpha>[^<]*<idle:beta>'
# The default view keeps its order: the drag rearranged only the view this
# attachment shows.
send-attach key='alt+v'
wait-rendered contains='Tab Views' timeout=10000
send-attach key='d'
send-attach key='e'
send-attach key='f'
send-attach key='a'
send-attach key='u'
send-attach key='l'
send-attach key='t'
send-attach key='enter'
wait-rendered contains='default │  <idle:alpha>' timeout=10000
assert-rendered matches='<idle:alpha>[^<]*<idle:beta>[^<]*<active:gamma>'
