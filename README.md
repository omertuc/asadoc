# Asadoc

Asadoc (read: as-a-doc) is a utility that helps achieve some level of "Docs as
Code" by retrofitting existing code/automation to match existing docs through
comment markers.

Code is tested, docs are not. If we can detect drift between our code and our
docs, we can indirectly achieve testing for our docs and help keep them up to
date.

☣️Asadoc is **vibe-coded**. This README is not. Use with caution

# Assumptions

This is a rather specific situation, that might or might not make sense for
your project:

- You have some code - repo code, automation, configuration, tests, that
  resembles / has large portions of it copy-pasted into your docs*. Commands to
  run, configuration files, etc.
- Your code changes often and gets tested automatically
- Your docs constantly drift, have mistakes and need manual intervention to
  keep up-to-date with code

*Our docs were very "config file" heavy and were basically a lot config copied
over from our automation. So this assumption is admittedly a bit far fetched. If
this is not the case for you, maybe this project is not for you.

# How it works

## Theory

We consider the following entities:

- Doc code blocks. Code blocks that appear inside the docs. Typically commands,
  command output or configuration files.
- "Code" (or configuration) files inside your project repos

Our goal is to associate doc code blocks to code files or portions of code files.

Doc code blocks are detected automatically by parsing the docs. Currently
Asadoc only supports the AsciiDoc format.

Every code block must be associated with code. No exception. If you can't or
don't want to associate it with code, you must declare it explicitly as
"ignored" in a special file.

## Markers

Association is done *inline*. We don't keep a map between docs and code. We
simply mark portions of our code, using special comment markers, as
docs-related.

Asadoc will automatically associate doc code blocks to code that matches them
*exactly*, content-wise.

Since an exact match is unlikely, as docs often differ in parameterization and
other subtle differences, Asadoc allows you to add special mutating functions
to your code markers that virtually transform either the doc or the code to
make them match

## UI

Asadoc offers a web-based UI to help you explore your docs and code and how
they relate and associate. In simple cases, it even offers to mark your code
for you to help it match the docs.

## CLI

Asadoc offers a CLI for use by scripts, CI and agents. The main use case would
be to ensure that all doc code blocks are accounted for through associations or
through ignoring.
