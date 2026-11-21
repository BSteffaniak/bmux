# Host artifact approval storage (source-stage contract)

`ArtifactApprovalStore::load(&ConfigPaths)` reads only
`config_dir/trusted-plugin-artifacts.toml` from the selected primary
configuration
root. It does not search candidate configuration directories, repository roots,
`BMUX_CONFIG`, CLI configuration overlays, or mutable state overlays. The caller
must supply the final resolved runtime paths, including slot/sandbox selection.
No file is created or modified. No real artifact is approved by this change.

The closed version-1 document has `schema_version = 1`, an absolute
`runtime_dir`
exactly matching the selected endpoint directory, and an explicit `approvals`
array (an empty array approves nothing). Each entry has exactly `plugin_id`,
`artifact_sha256` (64 hex digits), `role` (`backend`/`frontend`), and
`request_policy`. Duplicate logical components, unsupported schema versions,
invalid fields and runtime mismatches are errors. The generic config crate does
not know which application request policies exist. The application must validate
**all** selectors against its closed host composition before issuing any grant;
unknown/unsupported policy is an error, not a skipped entry.

On Unix, paths are opened component-by-component relative to directory handles.
Like IPC's SystemAncestors
policy, root-owned symlinks may be followed only before the final configuration
directory and beneath non-other-writable validated parents (for example macOS
`/tmp` and `/var`). User-controlled links, the final configuration directory,
and
the approval file retain no-follow behavior. No whole-path canonicalization or
candidate fallback occurs. Root/current-user ownership is required. Other-user
write permission is rejected; sticky ancestor directories are allowed, but the
final configuration directory cannot rely on that exception. The document must
be a regular non-other-writable file. Other platforms reject protected loading
until their security contract is implemented. Only genuine NotFound is absence;
malformed/unreadable/insecure authoritative state is not an empty store.

A configuration match is policy **intent**, not execution authority. A selected
native provider additionally needs an actual loader-issued receipt over retained
loaded bytes, exact role/identity and negotiated kernel-capable ABI. No receipt
can be fabricated from plugin metadata, manifest pins, logical names or a later
hash of the original pathname. Application `NativeKernelApproval::bind_created`
now checks the M3 native-only receipt: exact selected metadata, approved digest,
component role, execution runtime and instance, host-expected runtime, native
ABI
v3 and `CAP_KERNEL_REQUESTS`. Unverified loading paths return no receipt and are
rejected. Startup integration and loader type re-exports remain pending;
matching
APIs alone do not enable a native grant.

The initial application selector is `clients-kernel-v1`, resolving only through
host-compiled composition to the existing clients request policy, with backend
role and exact approved native artifact. It is not blanket foundational access,
and does not bypass principal authorization, runtime/instance binding,
cancellation,
admission, remote trust or authoritative side-effect checks. Missing approval
leaves the provider unprivileged; explicit mismatches/errors never cause bundled
fallback. An approval document for another named runtime is rejected, not
inherited.

Native plugins are trusted unsandboxed code. Main-library digests do not
authenticate
all transitive libraries or other package files. Whole-package trust requires
a
separate explicit package closure contract.

Tests are authored but intentionally unexecuted in this source stage.
Integration
and required repository validation remain pending; this is not shipped parity.
