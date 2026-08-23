@driver attach-sim
@viewport cols=100 rows=24

set-config path='appearance.status_position' value='off'
render

assert-rendered excludes='1:one'
assert-rendered excludes='2:two'
assert-rendered excludes='3:three'
