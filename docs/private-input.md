# Private input capture boundary

`bmux_tui::sensitive_input` owns bounded, zeroizing local input, not a normal
editor or semantic interaction snapshot. The public projection is independent
of secret contents and length. `TerminalInput::start_private` routes text before
ordinary runtime admission. Applications must not trace, clone, persist or
replay private delivery messages.

A `PrivateInputGate` requires an explicit `CaptureSuppression` owner.
Acquisition
quarantines ingress before requesting suppression; private focus is allowed only
after acknowledgment. Lease destruction wipes local input but does not restore
ordinary input. The ingress owner must drain raw, decoded and
application-pending
input before `drain_and_resume`; failure preserves quarantine. The trait is a
contract, not proof that an enclosing process has been suppressed.

The pane-runtime plugin exposes the generated, versioned
`attach-runtime-commands/suppress-pane-input-capture-v1` operation. It requires
an authenticated writable attachment, matching selected session and pane
membership. Its acknowledgment is serialized against raw recording copies and
sink writes. Suppression is absorbing for the process lifetime: there is no
resume command or timeout that could expose late password input. The bounded
registry refuses new suppression after 4,096 distinct panes rather than evicting
an existing protected pane. This intentionally trades continued recording for
privacy. Both ordinary and direct-pane input use the same barrier. Poisoned
barrier state rejects input rather than recording it.

Attach input diagnostics never include key codes or modifiers, including before
lease negotiation. Event classification counters can still reveal timing. No
claim is made to conceal traffic timing or input length from an observer of the
terminal/operating system.

## Threat model and integration requirements

These mechanisms protect cooperating BMUX input and recording paths. They do
not protect against an external terminal recorder, another multiplexer, a
malicious plugin, debugger, compromised OS, or previously running incompatible
BMUX binary. An application must not infer suppression from an environment flag
or a successful unrelated connection. It must obtain the owning pane's exact
suppression acknowledgment and establish its local ingress drain barrier before
accepting a credential. Unsupported or ambiguous owners require a secret-free
fallback. The owning pane also recognizes a bounded, fragmented OSC
`633;bmux-input-capture-v1;<fresh UUID>` request terminated with BEL. The
pane-runtime plugin consumes that request before normal output projection and
returns a bracketed-paste `bmux-input-capture-v1:` envelope containing generated
`InputCaptureAckV1` public data. Suppression is established before the reply is
written. A consumer must verify its fresh nonce and version, consume the reply
before composer routing, and still establish its own local input drain barrier.
This is a cooperating owning-PTY handshake, not cryptographic attestation: an
untrusted terminal or injected paste can forge an acknowledgment. Do not use
this protocol when the containing terminal/input path is untrusted. A complete
frontend handshake consumer/root-runtime integration is not yet provided; the
existence of the service/parser alone does not enable nested private entry.
