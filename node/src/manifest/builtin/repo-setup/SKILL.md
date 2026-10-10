---
name: repo-setup
description: Set this repository up once for the node, so its checks run in the right image after the right preparation, and say how to run the application. Use when a check fails for want of a tool or a dependency, when the node holds no entry for this repository, or when the operator asks for setup.
---

# Setting a repository up once

The node runs a repository's required checks from its `[[repo]]` entry: the image they
run in, the preparation that installs dependencies, the checks themselves, and the hosts
preparation may reach. An entry written once is what every later session on this
repository gets, so it is worth getting right rather than worked around in each task.

## When

- A required check fails because a tool, a toolchain or a dependency is missing.
- `repo_setup_draft` shows the node holds no entry for this repository, or one that no
  longer matches what the default branch holds.
- The operator asked you to set the repository up.

Do not use it to change what counts as passing. The checks are what the repository
already says must pass before a change is reviewed; leaving one out is not a fix.

## How

1. **Draft.** Call `repo_setup_draft`. It reads the default branch (a devcontainer,
   lockfiles, a `just check` recipe, `package.json` scripts, Cargo) and returns a draft,
   where each field came from, notes on what it could not settle, and the entry the node
   holds now. Read the notes: they are what you have to settle.
2. **Try.** Call `repo_setup_try` with no arguments to try the draft, or with the fields
   you changed. It prepares the default branch in a fresh container and runs each check
   the way required checks run, with no network. If it returns `running`, call it again
   with the `trial_id`.
3. **Fix what fails.** Read each failed step's exit and the end of its output.
   - A missing tool: a better `image` or the repository's `dockerfile`.
   - A fetch that fails during checks: move it into `prepare`, so it runs while the
     network is open.
   - A host preparation cannot reach: it is listed in `egress_not_tried`. Ask for it with
     `request_egress`; once the operator allows it, the next draft and trial include it.
   - A check that is slow: raise `timeout_secs` rather than dropping the check.
   Try again after each change. Stop when the trial passes, or when what is left is
   something only the operator can decide.
4. **Propose.** Call `repo_setup_propose` with the entry you tried. The operator answers
   on a card and may edit it first; nothing is written until they allow it. Read the
   answer with `approval_status`.
   - `why`: what the trial showed and anything you could not settle, in a few sentences.
   - `run_notes` (optional): how to run the application and give it data, as plain text
     for the next session. Which command starts it, what it listens on, which seed or
     fixture gives it data, what it needs that the checks do not. Only what you saw
     work. These become the operator's standing notes on this channel once allowed, so
     write them for someone who has never seen the repository, and leave out anything
     the code already says.

## What not to do

- Do not edit the node's configuration, the repository's CI, or a Dockerfile to make a
  trial pass without saying so in `why`.
- Do not propose an entry you have not tried. If you could not try it, say why.
- Do not put secrets, tokens or personal paths in any field or in `run_notes`.
