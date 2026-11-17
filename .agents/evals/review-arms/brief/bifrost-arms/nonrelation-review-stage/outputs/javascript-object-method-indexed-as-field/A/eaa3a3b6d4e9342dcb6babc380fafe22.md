- [P1] Avoid recursive full-tree walks for each call initializer — src/analyzer/typescript/mod.rs:1075-1077
  For every identifier call initialized with an object argument, this walks the entire syntax tree recursively; sufficiently deep TypeScript syntax can overflow the process stack during workspace initialization, and files with many such calls incur repeated full-file traversals. The repository explicitly requires stack-safe traversal on declaration collection paths ([AGENTS.md](/workspace/b31b9d07eb1205dddfa7ba6031f333b5/bifrost-dev/AGENTS.md:41)).

- [P2] Do not treat Zod schema keys as direct object members — src/analyzer/typescript/mod.rs:1020-1020
  For `const schema = z.object({ name: z.string() })`, this indexes `schema.name`, although `z.object` returns a schema whose field definitions are under `schema.shape`. Consequently a lookup of `schema.name` reports a nonexistent member as resolved; this also applies to any unrelated local object named `z` with an `object` method.

- [P2] Resolve the called function in its lexical scope — src/analyzer/typescript/mod.rs:1036-1040
  When functions with the same name exist in different scopes, any shape-preserving function anywhere in the file causes the call to be classified as shape-preserving. For example, an inner `function wrap(x) { return x; }` makes an unrelated outer `wrap({foo: 1})` index `foo` even if the outer function returns a number, producing false member definitions.

- [P2] Require all return paths to preserve the argument shape — src/analyzer/typescript/mod.rs:1152-1154
  A function such as `function wrap(x) { if (condition) return x; return {}; }` is considered shape-preserving because one return matches. A call `wrap({foo: 1})` then resolves `foo` to the argument even on the branch where the returned object has no such property; the same problem occurs when a later spread property overrides an argument property.

- [P2] Respect receiver shadowing when resolving members — src/analyzer/usages/get_definition.rs:813-820
  If a function parameter or local variable shadows a same-file object, such as `const config = {port: 1}; function f(config) { return config.port; }`, the file-wide identifier lookup selects the outer object and reports its `port` as the definition of the parameter's member. The new resolution path therefore returns an incorrect definite result for ordinary shadowed names.
