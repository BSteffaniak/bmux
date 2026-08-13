@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='alpha,beta,gamma' active='alpha'
render

# Default template shows only the window name, with no "N:" index prefix.
assert-rendered contains='alpha'
assert-rendered matches='\balpha\b'
assert-state path='status.tab_labels' equals='["alpha","beta","gamma"]'

# Legacy indexed template can be restored explicitly.
set-config path='status_bar.tab_template' value='{index}:{name}'
render
assert-rendered contains='1:alpha'
assert-rendered contains='3:gamma'
assert-state path='status.tab_labels' equals='["1:alpha","2:beta","3:gamma"]'

# Templates may add arbitrary chrome and an active marker.
set-config path='status_bar.tab_template' value='[{name}{marker}]'
render
assert-rendered contains='[alpha*]'
assert-rendered contains='[beta]'
assert-state path='status.tab_labels' equals='["[alpha*]","[beta]","[gamma]"]'

# Clicking still targets the templated tab.
locate id='beta' text='beta'
terminal-event kind=mouse phase=down button=left col='${beta.center_col}' row='${beta.row}'
terminal-event kind=mouse phase=up button=left col='${beta.center_col}' row='${beta.row}'
assert-effect operation='switch-window'
assert-state path='windows.active_name' equals='"beta"'
