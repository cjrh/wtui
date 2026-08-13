# wtui

A desktop app to manage Git worktrees.

## Features

- Add and save Git repositories.
- List each repository's worktrees and branch state.
- Show dirty and ignored file counts, upstream, and ahead/behind status.
- Create a new branch and worktree.
- Remove a worktree, with a warning before forced removal.
- View the selected worktree's GitHub pull request and open it in your browser.
- Remove a worktree for a merged pull request and delete its local branch.

Repository paths are saved in the platform configuration directory. On Linux, this is normally `$XDG_CONFIG_HOME/wtui/repositories.json` or `~/.config/wtui/repositories.json`.

## Requirements

- Rust and Cargo with Edition 2024 support.
- Git in `PATH`.
- Linux with X11 or Wayland support.
- [GitHub CLI](https://cli.github.com/) (`gh`) in `PATH` to show pull-request data. Other worktree actions do not require it.

This project uses local path dependencies. Clone these repositories beside `wtui` before you build:

```text
projects/
├── iced-dev-automation/
├── iced-themer/
└── wtui/
```

```sh
git clone https://github.com/cjrh/iced-themer.git
git clone https://github.com/cjrh/iced-dev-automation.git
git clone https://github.com/cjrh/wtui.git
cd wtui
cargo run
```

## Use

1. Start the app with `cargo run`.
2. Select **Add repository** and choose a checked-out Git repository.
3. Select a repository tab to inspect its worktrees.
4. Select **Add worktree** to create a branch and a sibling worktree directory.
5. Select a worktree to view its status and pull request.

The app refreshes repository state every five seconds. Use **Refresh** to update it now.

## License

[GPL-3.0-only](LICENSE)
