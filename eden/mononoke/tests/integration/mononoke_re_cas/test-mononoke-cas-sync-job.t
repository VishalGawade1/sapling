# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setup_common_config

  $ start_and_wait_for_mononoke_server
  $ hgmn_init repo
  $ cd repo
  $ drawdag << EOS
  > D # D/bar = zero\nuno\ntwo\n
  > |
  > C # C/bar = zero\none\ntwo\n (renamed from foo)
  > |
  > B # B/foo = one\ntwo\n
  > |
  > A # A/foo = one\n
  > EOS

  $ hgmn goto A -q
  $ hgmn push -r . --to master -q --create

  $ hgmn goto B -q
  $ hgmn push -r . --to master -q

  $ hgmn goto C -q
  $ hgmn push -r . --to master -q

  $ hgmn goto D -q
  $ hgmn push -r . --to master -q

Check that new entry was added to the sync database. 4 pushes
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "select count(*) from bookmarks_update_log";
  4

Sync all bookmarks moves
  $ with_stripped_logs mononoke_cas_sync repo 0 | grep -v "use case"
  Initiating mononoke RE CAS sync command execution for repo repo, repo: repo
  using repo "repo" repoid RepositoryId(0), repo: repo
  syncing log entries [1, 2, 3, 4] ..., repo: repo
  log entry BookmarkUpdateLogEntry * is a creation of bookmark, repo: repo (glob)
  log entries [1, 2, 3, 4] synced (4 commits uploaded, upload stats: *, repo: repo (glob)
  queue size after processing: 0, repo: repo
  successful sync of entries [1, 2, 3, 4], repo: repo
  Finished mononoke RE CAS sync command execution for repo repo, repo: repo
