# How asadoc works

asadoc finds code blocks in documentation and ensures all of them are either
matched to actual code in code repos or explicitly ignored. No docs are edited,
the point is just to detect drift to eventually drive doc PRs.

## Docs

The docs included in the analysis are the docs referenced by `.asadoc/config.toml`.

### Doc Code Blocks

Every code block in the docs. A block is referred to as `<docs source>:<docs
file>/<lang>-<NNN>`, e.g. `openshift-docs:nw-dpf-creating-bfb/yaml-001` is the
first YAML block in the docs file `nw-dpf-creating-bfb` of the docs source
`openshift-docs`. That name is only the block's current position. It's not
stored anywhere and assumed to always change.

### Code

The code is the repo `.asadoc/config.toml` is in, plus any other repos it lists
as `[[code]]`. Files ignored by SCM are ignored. A file is referred to by its
path in its repo, prefixed with the repo's name for a `[[code]]` repo:
`installer:deploy/bfb.yaml`.

## Marked code

Doc code blocks are only matched to marked code. Marked code is every code
marked in `#` comments (more comment styles might be supported later):

- A section: the lines between a start and an end marker.

  ```bash
  ...

  # @code-as-a-doc: start section "ip-forwarding-patch"
  oc patch network.operator.openshift.io cluster --type=merge -p \
    '{"spec":{"defaultNetwork":{"ovnKubernetesConfig":{"gatewayConfig":{"ipForwarding":"Global"}}}}}'
  # @code-as-a-doc: end section "ip-forwarding-patch"

  ...
  ```

- Whole file: a line with `# @code-as-a-doc: file` marks the entire file. A
  whole file for now cannot contain sections within it.

  ```yaml
  # @code-as-a-doc: file
  apiVersion: provisioning.dpu.nvidia.com/v1alpha1
  kind: BFB

  ...
  ```

The file marker and the start marker take options, separated by `|`, that
adapts either its code or any docs trying to match it. By default, options
modify the code. Use the "doc" keyword before an option to make the option
apply to the docs instead. For example:

```bash
    # @code-as-a-doc: start section "dpu-worker-config-install"
    #   | doc strip-line-prefix: "$ "
    #   | remove-prefix: "if " | remove-suffix: "; then"
    #   | unindent-common
    #   | param: "${*}"
    if helm upgrade --install dpu-worker-config \
        ...
        ${version_flag}; then
    # @code-as-a-doc: end section "dpu-worker-config-install"
```

Options are:

- `remove-prefix: "<text>"` strips `<text>` from the start of the first line
  (after its indentation).
- `remove-suffix: "<text>"` strips `<text>` from the end of the last line.
- `strip-line-prefix: "<text>"` strips `<text>` from the start of every line
  that has it; `doc strip-line-prefix: "$ "` drops the doc's shell prompts.
- `remove-lines-starting-with: "<text>"` drops lines whose text (after
  indentation) starts with `<text>`, e.g. `"#"` for comments on one side only.
- `remove-text: "<regex>"` removes every match of `<regex>`, which may span
  lines (`\n`). It's for text only one side has, wherever it sits, when no
  more specific option fits. Keep the regex as narrow as the text it targets.
  When it matches nothing, the doc block doesn't match (on the doc side) or
  it's a marker problem (on the code side).
- `remove-blank-lines` drops lines that are empty or only whitespace.
- `unindent-common` removes the indentation all non-blank lines share.
- `reindent: <from> -> <to>` turns each `<from>` spaces of leading indentation
  into `<to>` spaces. With `reindent: 4 -> 2`, 8 spaces become 4; spaces beyond
  a multiple of 4 are kept, so 6 become 4.
- `param: "<text>"` (code side only) declares `<text>` a placeholder (like
  `${VERSION}`): the doc can have anything without whitespace in its place,
  either an explicit value or a placeholder written in a different way. So
  `tag: ${VERSION}` matches `tag: 4.22` or `tag: $VERSION` but not `tag: 4.22
  (latest)`.
- `comment: "<text>"` is a note for people reading the marker; it changes
  nothing.
- `TODO: "<text>"` is a note of work left on this marked code. It changes
  nothing either, but `asadoc todo` lists every `TODO` with its file and
  line.

A `*` in a `param` or `remove-lines-starting-with` value matches any text
without whitespace. So `param: "${*}"` makes every `${...}` in the
marked code a placeholder, and `remove-lines-starting-with: "${*}"` drops
every line that starts with a `${...}`.

`param: "${**}"` is like `param: "${*}"`, but may contain whitespace.

Using `*` in params is preferred over listing every parameter explicitly, when
possible.

In a file that a tool like `envsubst` or `sed` fills values into, prefer
wildcards to named params: the tool would overwrite a named param in the marker
itself.

### Writing good markers

The point is to find drift, not to make `asadoc check` pass. If the doc or the
code is actually wrong, or matching them would mean changing what the code does,
leave the block unresolved and report it (or, once it's settled that the docs
are wrong, have it await a doc fix: see below).

1. Change the code where that doesn't make it worse. Arbitrary formatting
   (indentation, key order) can just match the doc. Code embedded in other code,
   like a template inside a script, can often move to its own file and be
   marked whole. Don't twist code into the doc's shape when that would make it
   worse or odd next to the code around it, e.g. the code is auto-formatted, or
   the doc has comments meant for doc readers, not code readers. Use an option
   instead.
2. Cover what's left with as few options as possible, each as narrow as
   possible. Prefer code-side options: doc-side ones are hard to reason about,
   since the docs aren't in front of whoever reads the marker, and they apply
   to every doc block matched against this code.
3. Run `asadoc check` until the block matches.
4. Go through every option on the marker, and any text a param covers beyond
   the placeholder itself, and ask whether a change to the code or the docs
   would make it unnecessary. If one would, add a TODO naming that change. If
   only a new asadoc feature would, add a TODO proposing it in general terms.
   The marker isn't done until every option has been through this.

## Ignored blocks

Doc blocks that don't have / need a representation in the repo are ignored by
putting their exact content in a file under `.asadoc/ignore/<reason>/`
(`.asadoc/ignore` configurable through `ignore_dir`). asadoc comes with a few
built-in reasons:

- `example-output/`: Docs often contain code blocks that simply show an example output of a terminal command. These of course usually have no correspondence in a repo so should be ignored.
- `manual-command/`: Docs often ask the user to run a command. If this command is long and complicated, maybe we also already have it in the code repo, so it should be marked and matched. But if it's simple (e.g. 'kubectl get pods') or specific to the docs, it should probably be ignored.
- `no-repo-source/`: Some code blocks in the docs don't have a counterpart in the code repo, so they should be ignored.

Any additional subdirectory is a "reason" too, and its `README.md` says what it
means (a `README.md` in a built-in reason directory replaces its description):

Each file holds the block's content to ignore verbatim. File names are
arbitrary: one file covers every block which has the same content as it.

## Awaiting a doc fix

When the code changes on purpose and a doc block it matched is now out of date,
the docs need a fix, which can take a while to land. Until then, the block can
**await a doc fix** instead of failing `asadoc check`:

```bash
asadoc await-doc-fix '<block>' --fix pull-secret-type \
  --description 'The docs should create the pull secret with --type=kubernetes.io/dockerconfigjson.
Tracked in https://github.com/openshift/openshift-docs/pull/12345'
```

This puts the block's exact content in a file under
`.asadoc/awaiting-doc-fix/<fix>/` (`.asadoc/awaiting-doc-fix` configurable
through `awaiting_doc_fix_dir`). Each subdirectory is one doc fix, and its
`README.md` (the `--description`) says what the docs need to change and where
that change is tracked. Several blocks can await the same fix: give them all, or
give `--fix` again without `--description` to add another one later.

`asadoc check` passes with blocks awaiting a doc fix, but still lists them,
with how each differs from its closest code. The entry only covers the content
the block had: once the docs change (and `ref` is moved to include the change),
the block is checked against the code again. Its old entry is then reported as
no longer needed, and its files can be deleted. The same happens when the code
matches the block again.

Only have a block await a doc fix once it's settled that the docs are what's
wrong. If it isn't (the code could be wrong, or it's unclear which one is), leave
the block to resolve and report it. If only the form differs, change the code or
the marker instead.

## Resolved, ignored, awaiting a doc fix, to resolve

A doc block is **resolved** when some marked code, with its options applied, is
byte-for-byte the block (with the marker's `doc` options applied) apart from
its placeholders. A block whose content is in a file under `.asadoc/ignore/` is
**ignored**. A block whose content is in a file under
`.asadoc/awaiting-doc-fix/` is **awaiting a doc fix**. Every other block is
still **to resolve**.

## Commands

```bash
asadoc --help
```
