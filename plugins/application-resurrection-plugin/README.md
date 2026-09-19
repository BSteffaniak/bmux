# Application resurrection adapter

Plugin-owned execution fencing, opaque descriptor validation, and revisioned
checkpoint transitions for the experimental `application-resurrection/1` protocol.

The plugin is statically bundled by default and can be disabled through normal
plugin configuration. Its status service currently reports `available = false`:
execution lifecycle hooks, durable checkpoint storage, and the authenticated local
socket listener are not connected yet. It intentionally does not advertise an
application endpoint before these safety requirements are met.

`execution` contains the tested transition model. A storage integration must commit
an update candidate before installing it and acknowledging its revision. Transport
disconnect must not be treated as execution exit. Host-authoritative command
boundaries invalidate registrations; manual launches never receive saved offers.
