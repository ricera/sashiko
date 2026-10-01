# RDMA/InfiniBand Subsystem Details

## Reading the Userspace Provider

An RDMA driver is half of an ABI. Its other half is a userspace provider
library that builds work requests, parses completions and decodes the
driver's private request and response structs, often from the same queue
memory the device reads. A kernel change that looks self-contained can break
every application on the machine because the provider still reads the old
layout, and nothing in the kernel tree shows it.

- If a userspace RDMA reference repository is listed under
  Read-only Reference Repositories, read the provider there at its pin rather
  than recalling it, and cite `repo:file:line` for every claim about it. The parts
  that matter are:
  - the provider's copy of the kernel uapi header, kept under
    `kernel-headers/rdma/` and synced byte-for-byte from the kernel's
    `include/uapi/rdma/`;
  - the provider's command wrappers (`DECLARE_DRV_CMD` and friends) that embed
    the uapi structs;
  - the provider's context setup, mmap calls, and its hand-kept header of
    device queue formats, which nothing syncs from the kernel.
- The synced header copy shows what the last sync contained, not what the
  provider does with it. Check the provider code that reads each field.
- If no such reference is listed, do not guess what userspace does. Flag any
  change to the uapi ABI structs, the context response fields, mmap offsets
  or device queue formats as needing a matching userspace provider change.
  Name the userspace files expected to change: the `kernel-headers/` copy of
  `<driver>-abi.h`, the provider's command wrappers, and the provider's
  device-format header.

## Provider ABI Structs (include/uapi/rdma/<driver>-abi.h)

The driver's uapi header is copied byte-for-byte into the userspace stack and
compiled into providers that are already installed. A moved offset, a changed
size or a reused field makes an installed provider send garbage the kernel
accepts, or read a response field from the wrong bytes. Neither fails loudly.

- New request or response fields may only claim reserved bytes (`rsvd`,
  `reserved`, padding) or be appended at the end. Offsets, sizes and the
  alignment of every existing field stay as they were. `__aligned_u64` fields
  keep their alignment, and an appended field must not add implicit padding
  between existing ones.
- No existing field is renamed, retyped, narrowed or given a new meaning. A
  rename alone breaks the provider's build at its next header sync.
- The driver's ABI version constant (the `uverbs_abi_ver` the provider is
  matched against) is bumped only for a deliberate break. A bump makes every
  installed provider fail to bind to the device.
- An older provider sends a shorter request and expects a shorter response:
  - Request fields beyond `udata->inlen` were not supplied and must be treated
    as zero. `ib_copy_from_udata()` copies exactly the length it is given and
    does not check `inlen`, so copying `sizeof(req)` from a shorter buffer
    reads past what userspace sent.
  - Request bytes beyond what this kernel understands must be checked zero,
    for example with `ib_is_udata_cleared()`, so that a newer provider asking
    for a feature this kernel lacks is refused rather than ignored.
  - Responses are copied with `min(sizeof(resp), udata->outlen)`.
    `ib_copy_to_udata()` also copies exactly the length it is given, and an
    unbounded copy overruns the shorter buffer of an older provider.
- In the provider, check the command wrappers that embed these structs and
  every place that fills a request field or reads a response field the patch
  touches.

**REPORT as bugs**: a request or response struct that grows without the copy
being bounded by `udata->inlen` or `udata->outlen`; a field moved out of
reserved space or placed where it changes an existing offset; a reserved
field that is newly given meaning without the kernel checking it was zero
before.

## Context Response Capabilities and Versions

The response to context allocation is where a driver tells its provider what
the device can do and how to talk to it. A field the provider reads
differently from how the kernel means it makes userspace use a queue type,
opcode or page size the device does not support. A version outside the range
the provider accepts makes the provider refuse the device outright.

- Version fields: the provider accepts a range (minimum and maximum version
  constants in its own headers). A kernel change that bumps the version it
  reports must keep it inside the range shipped providers accept, or must be
  paired with a provider change and documented as requiring it.
- Capability masks, queue type values, opcode tables, page shift and stride
  or size limits: check that the provider reads each changed field with the
  same width, units and meaning. A limit reported in bytes and read as a
  shift, or a mask with a bit reused for a new meaning, is silent.
- A new capability must be reported as a bit or field that an older provider
  ignores, never by changing the meaning of a value older providers already
  interpret.

## mmap Offsets and Pages

Doorbells, clock or timestamp pages and on-device queue memory reach
userspace through mmap offsets returned in responses. A wrong offset, page
size or protection gives userspace the wrong page -- another process's
doorbell, or a write mapping of a page the device reads -- or makes the mmap
fail for every application.

- Offsets are created with `rdma_user_mmap_entry_insert()` and looked up with
  `rdma_user_mmap_entry_get_pgoff()`; I/O memory is mapped with
  `rdma_user_mmap_io()`. Check that each entry lives as long as the object
  the offset was handed out for, and is removed with it.
- Check the offset encoding and the page size on both sides. A provider that
  computes an offset from a page shift reported in the context response
  breaks when that shift changes.
- A page the kernel maps read-only, by refusing `VM_WRITE` and clearing
  `VM_MAYWRITE` on the vma, must be mapped `PROT_READ` by the provider, or
  the provider's mmap fails. Changing an existing mapping from writable to
  read-only is a break for shipped providers.

## Device Queue Formats Mirrored by the Provider

The provider builds work queue entries, rings doorbells and parses completion
entries directly in queue memory. Their layouts live in a driver-private
header, not in uapi, and the provider keeps a hand-written copy that no sync
updates. A change on one side only is not caught by either build, and shows
up as wrong completions or a hung queue.

- Check bit shifts and masks, field widths and the endianness of every
  changed WQE, CQE or doorbell field.
- Check the color or phase bit that marks a CQE as valid, and how the queue
  wraps, on both the producer and consumer side.
- Check opcode numbering, CQE type and status values, and the reserved bytes
  of CQEs. A new CQE field must land in bytes that older providers ignore.
- Check both directions: the side that builds an entry and the side that
  parses it.

## New Objects, Flags and Compatibility

The kernel and its userspace stack are upgraded independently. A newer kernel
must work with an older provider and an older kernel with a newer provider. A
change that forces both to be upgraded together breaks every distribution
that ships them separately.

- A new feature is advertised through a capability bit, `comp_mask` bit or
  version-gated field, and the provider uses it only when advertised. Never
  assume the other side has it.
- The kernel rejects request flags and `comp_mask` bits it does not know,
  with `-EOPNOTSUPP` or `-EINVAL`, rather than ignoring them. Ignoring them
  makes a newer provider believe a feature is active when it is not.
- A new feature normally pairs a kernel series with a userspace change: the
  provider code, any direct-verbs API and its symbol version map, man pages,
  and a sync of the kernel headers. If the patch adds such a feature, check
  that the kernel half stands on its own when no provider uses it.

## Generic uverbs Paths

Changes to the shared uverbs layer reach every provider, not just one. A
changed attribute, or a changed udata chain, breaks libraries that are not
part of the patch at all.

- Driver udata is chained behind the core command structs; ioctl methods use
  attribute bundles (check presence with `uverbs_attr_is_valid()` before
  reading an optional attribute).
- Shared structs such as `ib_uverbs_clock_info` are read by more than one
  provider. A change to one is a change to all of them.
- When a userspace reference is available, check how the userspace command
  layer fills the changed attributes (its `kernel-headers/rdma/ib_user_ioctl_*.h`
  copy and the command wrappers that build the requests).

## Quick Checks

- **uapi copy in the tree under review**: an out-of-tree driver may keep its
  own copy of `<driver>-abi.h`. A change to one copy and not the other, or a
  reserved byte claimed differently in each, is a conflict.
- **Response defaults**: every new response field must be filled in on every
  path that copies the response, including error and fallback paths, so an
  uninitialized kernel stack byte never reaches userspace.
- **Kernel-side validation of userspace values**: queue sizes, strides and
  indexes read from a request are untrusted and must be bounded before use.
- **Feature removal**: dropping a capability or response field that shipped
  providers read is a break, even if no in-tree user remains.
