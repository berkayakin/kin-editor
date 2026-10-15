# Git

Kin uses the `git` command line, so your config, credential helpers, SSH keys and hooks
all apply. Prompts are disabled, so a remote that needs a password fails fast with a
message instead of hanging; use an SSH agent or a credential helper.

- **Gutter:** green/blue/red bars mark added, changed and deleted lines against `HEAD`,
  updated as you type; staged changes are drawn faded. Click a bar to see the lines it replaced
  inline, above the new ones (Escape hides them), and **Stage** (or **Unstage**) just that change,
  or **Rollback** it. Stage Lines / Unstage Lines are
  also in the editor's right-click menu and Find Action, and stage what the editor shows, unsaved
  edits included.
- **Inline blame:** the caret line shows its author, age and commit message
  (Find Action → Toggle Inline Git Blame).
- **File tree:** changed files are colored with a status letter; folders containing
  changes are tinted.
- **Commit panel** (⌘K / ⌘0, or the Commit tab above the sidebar, whose tooltip shows the
  number of changes): the branch with fetch, pull and push at the top, then staged and unstaged
  changes with stage, unstage, open and roll back on hover; click a file for its diff. Write a message and press ⌘↩ to commit (with nothing
  staged, everything is committed), or use **Commit & Push**. **Amend** starts from the
  previous message. In a folder that isn't a repository it offers **Initialize Repository**.
- **Diffs** show side by side or unified (the choice is remembered), with syntax colors and the
  changed words within a line highlighted. Each change has **Stage** / **Unstage**; F7 / ⇧F7 step
  through changes; collapsed unchanged lines expand on click. Images show the previous and new
  version side by side; other binary files say there's no text diff.
- **Branches:** the title bar shows the current branch next to the project name; the Commit
  panel's header shows it too, with the number of commits to pull and to push on its Pull and
  Push buttons. Click the branch (or use **Branches…** in the Git
  menu) to switch to a local or remote branch, or type a new name to create one. Hover another branch
  for **Merge** (into the current one) and **Rebase** (the current one onto it), and a local one
  for **Rename** and **Delete** (Kin says when a branch has commits no other branch has, and
  deletes it anyway only if you confirm).
- **Stash:** **Stash Changes…** (the archive button on the Commit panel's first section, the Git
  menu, Find Action) stashes every change, untracked files included, with an optional message.
  Stashes are listed under the changes; hover one to **Pop** (apply and drop), **Apply** (keep it)
  or **Drop** it (after asking). A stash that conflicts leaves the files conflicted and the stash kept.
- **Merge conflicts:** a merge or rebase that stops on conflicts shows a banner in the Commit
  panel with **Abort**. Conflicted files open in the editor, where each conflict is tinted
  (current side green, incoming blue) with **Accept Current / Accept Incoming / Accept Both**
  on its first line (also in the right-click menu and Find Action). Mark a file resolved with
  ✓ on hover (Kin asks if markers remain), then **Commit Merge** / **Continue**. The merge
  message is filled in, and committing is blocked while conflicts remain.
- **Log** (⌘9 / Alt+9, or its button at the top right): commits of all branches (toggle) with a branch
  graph, branch and tag labels, and a filter by message, author, hash or branch. Select a commit
  for its message, author, date, hash (copy) and changed files; click a file for its diff.
  Right-click a commit to copy its hash, **Checkout Revision** (detached HEAD), create a **New
  Branch…** or **New Tag…** there, **Cherry-Pick** it onto the current branch, **Revert** it, or
  **Reset Here** soft (changes kept staged), mixed (kept unstaged) or hard (discarded, after
  asking). A cherry-pick or revert that conflicts stops like a merge, with Abort / Continue.
  **Show History** (editor right-click menu, Git menu) lists one file's commits, following renames.
- **Push / Pull / Fetch:** from the Commit panel, the keyboard, or Find Action. Pushing a
  new branch sets its upstream. Results appear as notifications.
- **Remotes:** **Manage Remotes…** (Git menu, Find Action, or the cloud button in the Push
  dialog) lists each remote with its URL; add one, or hover one to **Edit** its name and URL or
  **Remove** it (after asking; the remote repository isn't touched). A new branch is pushed to
  `origin`, else the first remote; with several, the Push dialog lets you pick.
- **Push** (⌘⇧K / Ctrl+Shift+K) first opens a dialog with the branch and where it goes (a new
  branch is marked, with a choice of remote when there are several), and the commits to push. Click a commit for its message and files, and a
  file for its diff (closing it returns to the dialog). It warns when the remote has commits you
  don't have; **Force push** replaces them only if the remote hasn't moved since your last fetch
  (`--force-with-lease`). Enter pushes, Esc cancels.

- **Clone:** **Clone Repository…** on the welcome screen, in the File and Git menus, or Find Action
  (also as "Get from Version Control"). Enter an HTTPS or SSH URL (one on the clipboard is
  filled in) and a parent folder; progress is shown live, **Stop** cancels and removes the partial
  folder, and the clone opens when it's done. **Shallow clone** fetches only the latest commit.
- **Welcome screen:** with no tabs open, it shows the current branch, ahead/behind counts,
  changed files, the last commit and the remote (click to open it in the browser), with
  Commit, Update, Push, Fetch and Branches. In a folder that isn't a repository it offers
  **Initialize**.

Status refreshes after saves and Git actions, when the window regains focus, and every few
seconds. Open files without unsaved edits reload when a checkout, pull or rollback changes
them on disk.
