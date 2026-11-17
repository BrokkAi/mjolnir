- [P2] Substitute parameters inside qualified return types — src/analyzer/usages/get_definition.rs:7384-7391
  When a wrapper declares `const T* operator->()` (or another return type containing `T` rather than exactly equal to it), the pointee text is `const T`, so the exact-string comparison never substitutes the alias argument. Resolving that text then searches for a type named `T` instead of the actual pointee, causing member lookup through aliases such as `using P = Wrapper<Node>` to fail even though the previous first-argument resolution found `Node`.

- [P2] Follow operator-> results that are class types — src/analyzer/usages/get_definition.rs:7361-7364
  When `operator->` returns a proxy object whose own `operator->` returns a pointer, C++ applies the operator repeatedly until it reaches a pointer. Rejecting the first non-pointer return breaks valid wrappers such as `Wrapper<Node>::operator->() -> Proxy<Node>` and regresses aliases that previously resolved to their first template argument.

- [P2] Avoid looking up arrow members on the wrapper after failure — src/analyzer/usages/get_definition.rs:6420-6425
  If an aliased template wrapper has no `operator->`, this falls through to `cpp_type.unit`, so `p->member` can resolve to a member of the wrapper itself even though the expression has no valid arrow operation. Previously an alias with a resolvable first template argument used that argument as the owner, so this change newly produces false definitions for wrapper members.
