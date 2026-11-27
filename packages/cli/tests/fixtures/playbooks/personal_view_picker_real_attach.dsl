@driver real-attach
@timeout 300000
@viewport cols=120 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/personal_view_picker_real_attach.toml
new-session name='bootstrap'
send-attach key='alt+a'
wait-rendered contains='<active:alpha>' timeout=60000
send-attach key='alt+b'
wait-rendered contains='<active:beta>' timeout=60000
# Rename beta for this view only: the strip shows the label.
send-attach key='alt+l'
wait-rendered contains='<active:mine>' timeout=60000
# The view picker creates a copy of this view, named work, and switches to it.
send-attach key='alt+v'
wait-rendered contains='Tab Views' timeout=60000
send-attach key='f'
send-attach key='r'
send-attach key='o'
send-attach key='m'
wait-rendered contains='New view from this one' timeout=60000
send-attach key='enter'
wait-rendered contains='New Tab View' timeout=60000
send-attach key='w'
send-attach key='o'
send-attach key='r'
send-attach key='k'
send-attach key='enter'
wait-rendered contains='View: work' timeout=60000
assert-rendered matches='<idle:alpha>[^<]*<active:mine>'
# Work keeps its own arrangement: removing the labelled tab there leaves the
# default view alone.
send-attach key='alt+x'
wait-rendered contains='<active:alpha>' timeout=60000
# Back to the default view through the picker: the tab and its label are
# still there, and the bar no longer names a view.
send-attach key='alt+v'
wait-rendered contains='Tab Views' timeout=60000
send-attach key='d'
send-attach key='e'
send-attach key='f'
send-attach key='a'
send-attach key='u'
send-attach key='l'
send-attach key='t'
send-attach key='enter'
wait-rendered contains='<idle:mine>' timeout=60000
assert-rendered matches='<active:alpha>[^<]*<idle:mine>'
# Clearing the label shows the shared name again, in this view.
send-attach key='alt+j'
wait-rendered contains='<active:mine>' timeout=60000
send-attach key='alt+k'
wait-rendered contains='<active:beta>' timeout=60000
