/**
 * Pure helpers for the `brain0` npm CLI (no I/O, unit-tested): repo-id inference from a git
 * remote, argument parsing, and the shared repo-id validation mirrored from the server.
 */

/** Legal repo id (mirrors the server's refresh validation — keep in sync). */
export const REPO_RE = /^[A-Za-z0-9._/-]+$/;

/** Replace illegal characters and strip leading dashes so the result always passes REPO_RE. */
export function sanitizeRepoId(raw) {
  const cleaned = String(raw)
    .replace(/[^A-Za-z0-9._/-]+/g, "-")
    .replace(/^-+/, "");
  return cleaned || "repo";
}

/**
 * Infer a stable repo id from the git remote URL, falling back to the directory name.
 * Handles the common shapes:
 *   git@github.com:owner/repo.git · ssh://git@host/owner/repo.git
 *   https://host/owner/repo(.git) · https://host/group/sub/repo (GitLab nesting)
 */
export function inferRepoId(remoteUrl, dirName) {
  const url = (remoteUrl ?? "").trim();
  if (url) {
    let path = "";
    const scp = url.match(/^[\w.+-]+@[^:/]+:(.+)$/); // git@host:owner/repo.git
    if (scp) {
      path = scp[1];
    } else {
      try {
        path = new URL(url).pathname; // ssh:// https:// git:// file://
      } catch {
        path = "";
      }
    }
    path = path.replace(/^\/+/, "").replace(/\/+$/, "").replace(/\.git$/, "");
    if (path) {
      // Keep the last two segments (owner/repo) — enough identity without host noise.
      const segs = path.split("/").filter(Boolean);
      const id = segs.slice(-2).join("/");
      if (id && REPO_RE.test(id) && !id.startsWith("-")) return id;
      return sanitizeRepoId(id);
    }
  }
  return sanitizeRepoId(dirName ?? "repo");
}

/**
 * Minimal argv parser: `--flag` booleans, `--opt value` pairs, everything else positional.
 * `optNames` decides which --names consume a value; unknown --names are flags.
 */
export function parseArgs(argv, optNames) {
  const takesValue = new Set(optNames);
  const flags = new Set();
  const opts = new Map();
  const positional = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const name = a.slice(2);
      if (takesValue.has(name) && i + 1 < argv.length) {
        opts.set(name, argv[++i]);
      } else {
        flags.add(name);
      }
    } else {
      positional.push(a);
    }
  }
  return { flags, opts, positional };
}
