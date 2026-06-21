#!/usr/bin/env python3
"""List and prune GHCR container packages for the Atlas Chain org.

Reads `GH_TOKEN` from the environment for `gh api` calls. The org defaults to
`atlas-chain` and can be overridden via `--org` or the `ORG` env var.

Versions tagged only by commit hash (or untagged) are considered ephemeral and
expire after `--lifetime-hours` (default 72). Listing always shows the time
remaining; `--prune` opts into actually deleting expired versions.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import subprocess
import sys
import urllib.parse


DEFAULT_ORG = "atlas-chain"
DEFAULT_LIFETIME_HOURS = 72
COMMIT_TAG_MIN_LEN = 7


def gh_api(path: str, method: str = "GET", paginate: bool = False) -> object:
    cmd = ["gh", "api"]
    if paginate:
        cmd.append("--paginate")
    if method != "GET":
        cmd.extend(["-X", method])
    cmd.append(path)
    try:
        result = subprocess.run(cmd, capture_output=True, text=True)
    except FileNotFoundError:
        sys.stderr.write("error: `gh` CLI is required but not found on PATH\n")
        sys.exit(2)
    if result.returncode != 0:
        sys.stderr.write(f"gh api {method} {path} failed: {result.stderr.strip()}\n")
        return None
    body = result.stdout.strip()
    if not body:
        return None
    try:
        return json.loads(body)
    except json.JSONDecodeError:
        return None


def is_commit_tag(tag: str) -> bool:
    """Return True if `tag` looks like a git commit SHA (hex, len >= 7)."""
    if len(tag) < COMMIT_TAG_MIN_LEN:
        return False
    try:
        int(tag, 16)
    except ValueError:
        return False
    return True


def parse_created_at(value: str) -> dt.datetime | None:
    if not value:
        return None
    try:
        return dt.datetime.strptime(value, "%Y-%m-%dT%H:%M:%SZ").replace(
            tzinfo=dt.timezone.utc
        )
    except ValueError:
        return None


def fmt_remaining(delta: dt.timedelta) -> str:
    """Render a positive timedelta as e.g. '12h', '1h 30m', or '15m'."""
    total_minutes = int(delta.total_seconds() // 60)
    if total_minutes <= 0:
        return "0m"
    hours, minutes = divmod(total_minutes, 60)
    if hours and minutes:
        return f"{hours}h {minutes}m"
    if hours:
        return f"{hours}h"
    return f"{minutes}m"


def deletion_status(
    tags: list[str],
    created: dt.datetime | None,
    lifetime: dt.timedelta,
    now: dt.datetime,
) -> str:
    """Human-readable status describing whether/when a version will be deleted."""
    if tags and not all(is_commit_tag(t) for t in tags):
        return "tagged to release and won't be deleted"
    if created is None:
        return "deletion time unknown (missing created_at)"
    remaining = (created + lifetime) - now
    if remaining.total_seconds() <= 0:
        return "eligible for deletion now"
    return f"will be deleted in {fmt_remaining(remaining)}"


def list_versions(org: str, package: str) -> list[dict]:
    encoded = urllib.parse.quote(package, safe="")
    versions = gh_api(
        f"/orgs/{org}/packages/container/{encoded}/versions", paginate=True
    )
    if not isinstance(versions, list):
        return []
    return versions


def delete_version(org: str, package: str, version_id: int) -> bool:
    encoded = urllib.parse.quote(package, safe="")
    try:
        result = subprocess.run(
            ["gh", "api", "-X", "DELETE",
             f"/orgs/{org}/packages/container/{encoded}/versions/{version_id}"],
            capture_output=True, text=True,
        )
    except FileNotFoundError:
        sys.stderr.write("error: `gh` CLI is required but not found on PATH\n")
        return False
    if result.returncode != 0:
        sys.stderr.write(
            f"  ! delete failed for version {version_id}: {result.stderr.strip()}\n"
        )
        return False
    return True


def cmd_list(args: argparse.Namespace, gh_token: str) -> int:
    org = args.org
    package = args.package
    lifetime = dt.timedelta(hours=args.lifetime_hours)
    now = dt.datetime.now(dt.timezone.utc)

    print(
        f"Package: ghcr.io/{org}/{package}  "
        f"(commit-only tags expire after {args.lifetime_hours}h)"
    )
    versions = list_versions(org, package)
    if not versions:
        print("  (no versions found)")
        return 0

    tagged = [
        v for v in versions
        if v.get("metadata", {}).get("container", {}).get("tags")
    ]
    tagged.sort(key=lambda v: v.get("created_at", ""), reverse=True)

    if not tagged:
        print("  (no tagged versions)")
    else:
        for version in tagged:
            tags = sorted(version["metadata"]["container"]["tags"])
            created = parse_created_at(version.get("created_at", ""))
            date = version.get("created_at", "")[:10]
            status = deletion_status(tags, created, lifetime, now)
            print(
                f"  - {', '.join(tags)}  uploaded: {date}  ({status})"
            )

    if args.prune:
        prune(org, package, versions, lifetime, now)

    return 0


def prune(
    org: str,
    package: str,
    versions: list[dict],
    lifetime: dt.timedelta,
    now: dt.datetime,
) -> None:
    cutoff = now - lifetime
    hours = int(lifetime.total_seconds() // 3600)
    print(
        f"\nPruning versions of {package} with no human tags older than "
        f"{hours}h (before {cutoff.isoformat()}):"
    )

    candidates = []
    for v in versions:
        tags = v.get("metadata", {}).get("container", {}).get("tags") or []
        if tags and not all(is_commit_tag(t) for t in tags):
            continue
        created = parse_created_at(v.get("created_at", ""))
        if created is None or created >= cutoff:
            continue
        candidates.append((v, tags, created))

    if not candidates:
        print("  (nothing to prune)")
        return

    deleted = 0
    for v, tags, created in candidates:
        vid = v.get("id")
        label = ", ".join(tags) if tags else "<untagged>"
        date_str = created.strftime("%Y-%m-%d %H:%M:%SZ")
        if delete_version(org, package, vid):
            deleted += 1
            print(f"  - deleted version {vid} ({label}) created {date_str}")
    print(f"  pruned {deleted}/{len(candidates)} version(s)")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="packages.py",
        description="List GHCR container packages and optionally prune commit-only tags.",
    )
    parser.add_argument(
        "--org",
        default=os.environ.get("ORG", DEFAULT_ORG),
        help=f"GitHub org (default: env ORG or {DEFAULT_ORG})",
    )
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_list = sub.add_parser("list", help="List versions of a package")
    p_list.add_argument(
        "--package", required=True, help="Container package name (without org prefix)"
    )
    p_list.add_argument(
        "--lifetime-hours",
        type=int,
        metavar="HOURS",
        default=DEFAULT_LIFETIME_HOURS,
        help=(
            "Lifetime (in hours) for commit-only/untagged versions. Used to "
            "show when each version will be deleted, and as the cutoff for "
            f"--prune. Default: {DEFAULT_LIFETIME_HOURS}."
        ),
    )
    p_list.add_argument(
        "--prune",
        action="store_true",
        help=(
            "Delete versions older than --lifetime-hours that are either "
            "untagged or tagged only by commit hash (hex)."
        ),
    )
    p_list.set_defaults(func=cmd_list)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)

    gh_token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN") or ""
    if not gh_token:
        sys.stderr.write("error: GH_TOKEN (or GITHUB_TOKEN) must be set\n")
        return 2

    return args.func(args, gh_token)


if __name__ == "__main__":
    raise SystemExit(main())
