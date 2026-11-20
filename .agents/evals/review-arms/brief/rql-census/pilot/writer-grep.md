## Relations to look for

Use grep, the historical diff, and source reads to follow the changed
identifiers and their consumers. Use this checklist to decide which relations
belong in the brief:

- Find declarations, definitions, imports, references, implementations and
  overrides. Resolve same-name matches to their actual definitions.
- Follow callers and callees, then follow entry points, wrappers, handlers
  and other routes that can reach the changed code.
- Find field, key and configuration reads and writes. Trace written values
  and call arguments to the reader, parameter or setting that consumes them.
- Check contracts at call and implementation boundaries. Look for what
  callers provide or assume and what consumers actually require, including
  hold/release pairs and differences between implementations.
- Follow value flow from bindings and assignments through arguments,
  parameters and returns to destinations that store or consume the value.
- Follow guarded paths. Read branches, early returns, errors, cleanup,
  cancellation and retries to identify which guards and ordering apply at
  the sites you relate.

Read the surrounding code to establish each relation. Record it in the brief
with its source anchors.
