/*
 *  Copyright (c) 2016, Facebook, Inc.
 *  All rights reserved.
 *
 *  This source code is licensed under the BSD-style license found in the
 *  LICENSE file in the root directory of this source tree. An additional grant
 *  of patent rights can be found in the PATENTS file in the same directory.
 *
 */
#pragma once

#include "eden/fs/store/BackingStore.h"

#include <folly/Range.h>

struct git_oid;
struct git_repository;

namespace facebook {
namespace eden {

class Hash;
class LocalStore;

/**
 * A BackingStore implementation that loads data out of a git repository.
 */
class GitBackingStore : public BackingStore {
 public:
  /**
   * Create a new GitBackingStore.
   *
   * The LocalStore object is owned by the EdenServer (which also owns this
   * GitBackingStore object).  It is guaranteed to be valid for the lifetime of
   * the GitBackingStore object.
   */
  GitBackingStore(folly::StringPiece repository, LocalStore* localStore);
  virtual ~GitBackingStore();

  /**
   * Get the repository path.
   *
   * This returns the path to the .git directory itself.
   */
  const char* getPath() const;

  folly::Future<std::unique_ptr<Tree>> getTree(const Hash& id) override;
  folly::Future<std::unique_ptr<Blob>> getBlob(const Hash& id) override;
  folly::Future<std::unique_ptr<Tree>> getTreeForCommit(
      const Hash& commitID) override;

 private:
  GitBackingStore(GitBackingStore const&) = delete;
  GitBackingStore& operator=(GitBackingStore const&) = delete;

  std::unique_ptr<Tree> getTreeImpl(const Hash& id);
  std::unique_ptr<Blob> getBlobImpl(const Hash& id);
  std::unique_ptr<Tree> getTreeForCommitImpl(const Hash& commitID);

  static git_oid hash2Oid(const Hash& hash);
  static Hash oid2Hash(const git_oid* oid);

  LocalStore* localStore_{nullptr};
  git_repository* repo_{nullptr};
};
}
}
