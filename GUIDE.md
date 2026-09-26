# How asadoc works

asadoc checks that every code block in a set of AsciiDoc docs comes from code
in this repo. The docs are never edited: when the two disagree, the repo side
changes (its code, or the markers on it), or the doc block gets ignored.

## Doc blocks

Every `[source,…]` block in the modules included by the assemblies listed in
`asadoc.yaml`. A block's content is the lines between its `----` delimiters,
ending with a newline. Tools refer to a block as `<module>/<lang>-<NNN>`, e.g.
`nw-dpf-creating-bfb/yaml-001` is the first YAML block in
`modules/nw-dpf-creating-bfb.adoc`. That name is only the block's current
position; nothing stores it.

## Marked code

Code in the repo that appears in the docs, marked in `#` comments:

- A whole file: a line with `@docs-as-code: file`. The marked content is the
  file without that line (and its option lines). A file marked whole can't
  also have sections.

  ```yaml
  # @docs-as-code: file
  apiVersion: provisioning.dpu.nvidia.com/v1alpha1
  kind: BFB
  ```

- A section: the lines between a start and an end marker, markers excluded.

  ```bash
  # @docs-as-code: start section "ip-forwarding-patch"
  oc patch network.operator.openshift.io cluster --type=merge -p \
    '{"spec":{"defaultNetwork":{"ovnKubernetesConfig":{"gatewayConfig":{"ipForwarding":"Global"}}}}}'
  # @docs-as-code: end section "ip-forwarding-patch"
  ```

Marked content is whole lines, like a doc block's: a file's last line counts
as ending with a newline even when the file doesn't.

The file marker and the start marker take options, each starting with `|`,
that adapt code which can't read exactly like the doc where it sits. They can
follow the marker on the same line, or on the comment lines right after it
(the marked content starts after the last of them). An option applies to the
marked code; prefixed with `doc`, it applies to the doc blocks compared with
that code instead:

```bash
    # @docs-as-code: start section "dpu-worker-config-install"
    #   | doc strip-line-prefix: "$ "
    #   | remove-prefix: "if " | remove-suffix: "; then"
    #   | unindent-common
    #   | param: "${version_flag}"
    if helm upgrade --install dpu-worker-config \
        ...
        ${version_flag}; then
    # @docs-as-code: end section "dpu-worker-config-install"
```

- `remove-prefix: "<text>"` strips `<text>` from the start of the first line
  (after its indentation).
- `remove-suffix: "<text>"` strips `<text>` from the end of the last line.
- `strip-line-prefix: "<text>"` strips `<text>` from the start of every line
  that has it; `doc strip-line-prefix: "$ "` drops the doc's shell prompts.
- `remove-lines-starting-with: "<text>"` drops lines whose text (after
  indentation) starts with `<text>`, e.g. `"#"` for comments on one side only.
- `remove-blank-lines` drops lines that are empty or only whitespace.
- `unindent-common` removes the indentation all non-blank lines share.
- `reindent: <from> -> <to>` turns each `<from>` spaces of leading indentation
  into `<to>` spaces. With `reindent: 4 -> 2`, 8 spaces become 4; spaces beyond
  a multiple of 4 are kept, so 6 become 4.
- `param: "<text>"` (code side only) declares `<text>` a placeholder for a value
  the repo leaves open (like `<NODES_MTU>` or `${VERSION}`) and the doc spells
  out. It matches whatever the doc has there, within that line; if it appears
  more than once, the doc must have the same value everywhere. Repeat it for
  each placeholder.

In `param` and `remove-lines-starting-with`, `*` stands for any name (letters,
digits and `_`). `param: "<*>"` makes every `<NAME>` in the marked code a
placeholder, and `remove-lines-starting-with: "<*>"` drops lines that start
with one:

```yaml
# @docs-as-code: file
#   | param: "<*>" | remove-lines-starting-with: "<*>"
```

Prefer the wildcard to spelling placeholders out. Markers are part of the file,
so whatever fills in `<NODES_MTU>` in a template fills it in inside the marker
comment too; with a multi-line value, that breaks the file.

Options apply in the order written. Values are quoted exactly (use `\"` for a
quote). Nothing is adjusted unless an option says so. A marker that can't be
read, or an option that doesn't fit the code, is reported as a marker problem.

## Ignored blocks

Doc blocks that don't come from the repo are ignored by putting their exact
content in a file under `.asadoc-ignore/` (next to `asadoc.yaml`; `ignore_dir`
changes it), in a subdirectory for the reason:

- `example-output/`: sample output shown to the reader
- `manual-command/`: a command too simple or doc-specific to track
- `no-repo-source/`: content with no counterpart in the repo

```text
.asadoc-ignore/
  example-output/nw-dpf-worker-machineconfig--terminal-005.txt
  manual-command/nw-dpf-management-cluster-setup--terminal-002.txt
```

Each file holds the block's content verbatim. File names are only names:
`asadoc serve` names them after a block that had the content, and one file
covers every block with the same content.

## Resolved, ignored, to resolve

A doc block is **resolved** when some marked code, with its options applied, is
byte-for-byte the block (with the marker's `doc` options applied) apart from its
placeholders. A block whose content is in a file under `.asadoc-ignore/` is
**ignored**.
Every other block is still **to resolve**.

Nothing else links the two sides: there's no mapping to keep up to date, and a
block moving within the docs changes nothing. One piece of marked code can
resolve several doc blocks.

## Resolving a block

Make some marked code match it, changing only the repo:

1. Find the repo code the block comes from.
2. Mark it (file marker, or start/end markers around the relevant part).
3. Make the marked content match the block: fix real differences in the code,
   and use marker options for the rest (placeholders, shell prompts,
   `if … then` wrappers, indentation, comments).

Or ignore the block if it has no repo counterpart worth tracking.

Check a block with `asadoc check <ref>`. It reports `✓ resolved` (with the value
each placeholder took), or a diff of the block against the most similar repo
code with marker options applied (or, when nothing resembles it, the block
itself). `--against <code>` compares it with specific marked code instead.
`asadoc check <path>` does the same from the code side: whether the file's
marked code matches a block, and a diff against the closest one if not. Either
way, it exits 0 only when everything given is resolved or ignored.

When marking a file or lines of one (plus a `---` or trailing newlines) is all
a block needs, `asadoc check` lists the change under it, and `asadoc fix <ref>`
makes it.

## asadoc.yaml

```yaml
docs:
  asciidoc:                # the docs' format (only AsciiDoc, for now)
    git: https://github.com/openshift/openshift-docs
    ref: main              # branch, tag or commit; a commit pins the check
    assemblies:            # whose modules' code blocks must come from this repo
      - networking/dpf/dpf-operator-installation.adoc
links:
  repo: https://github.com/org/repo/blob/main/   # for "source" links in the UI
```

asadoc fetches just `ref` of the docs repo, and only the files it reads, into
`~/.cache/asadoc/`. Instead of `git` and `ref`, `path: ../openshift-docs` reads
a local checkout as it is on disk (and `asadoc serve` then follows its changes);
`--docs <dir>` does the same for one run, e.g. to try unmerged docs changes.
Links to the docs default to the fetched commit on GitHub (`links.docs`
overrides them).

## Commands

```bash
asadoc check                      # everything to resolve, unmatched marked code, marker problems
asadoc check <ref>...             # specific blocks, with a diff for unresolved ones
asadoc check <ref> --against <c>  # a block against specific marked code
asadoc check <path>               # marked code in a file, against its closest blocks
asadoc fix <ref>                  # make the simple change `check` lists under a block
asadoc serve                      # the review UI (http://localhost:3000)
asadoc guide           # this guide
```
