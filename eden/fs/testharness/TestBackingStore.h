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

#include <memory>
#include "eden/fs/store/BackingStore.h"

namespace facebook {
namespace eden {

class LocalStore;

/**
 * A BackingStore implementation for test code.
 */
class TestBackingStore : public BackingStore {
 public:
  explicit TestBackingStore(std::shared_ptr<LocalStore> localStore);
  virtual ~TestBackingStore();

  folly::Future<std::unique_ptr<Tree>> getTree(const Hash& id) override;
  folly::Future<std::unique_ptr<Blob>> getBlob(const Hash& id) override;
  folly::Future<std::unique_ptr<Tree>> getTreeForCommit(
      const Hash& commitID) override;

 private:
  std::shared_ptr<LocalStore> localStore_;
};
}
} // facebook::eden
