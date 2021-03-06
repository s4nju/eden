/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/Function.h>
#include <folly/Synchronized.h>
#include <algorithm>
#include <cstdint>
#include <memory>
#include <optional>
#include <unordered_map>
#include "eden/fs/journal/JournalDelta.h"
#include "eden/fs/service/ThriftUtil.h"
#include "eden/fs/service/gen-cpp2/StreamingEdenService.h"
#include "eden/fs/telemetry/EdenStats.h"

namespace facebook {
namespace eden {

/** Contains statistics about the current state of the journal */
struct JournalStats {
  size_t entryCount = 0;
  std::chrono::steady_clock::time_point earliestTimestamp;
  std::chrono::steady_clock::time_point latestTimestamp;
  size_t maxFilesAccumulated = 0;
  uint64_t getDurationInSeconds() {
    return std::chrono::duration_cast<std::chrono::seconds>(
               std::chrono::steady_clock::now() - earliestTimestamp)
        .count();
  }
};

struct JournalDeltaInfo {
  Hash fromHash;
  Hash toHash;
  JournalDelta::SequenceNumber sequenceID;
  std::chrono::steady_clock::time_point time;
};

/** The Journal exists to answer questions about how files are changing
 * over time.
 *
 * It contains metadata only; it is not a full snapshot of the state of
 * the filesystem at a particular point in time.
 * The intent is to be able query things like "which set of files changed
 * between time A and time B?".
 *
 * In the initial implementation we are recording file names from the overlay
 * but will expand this to record things like checking out different
 * revisions (the prior and new revision hash) from which we can derive
 * the larger list of files.
 *
 * The Journal class is thread-safe.  Subscribers are called on the thread
 * that called addDelta.
 */
class Journal {
 public:
  explicit Journal(std::shared_ptr<EdenStats> edenStats)
      : edenStats_{std::move(edenStats)} {
    /** Add 0 so that this counter shows up in ODS */
    edenStats_->getJournalStatsForCurrentThread().truncatedReads.addValue(0);
  }

  /// It is almost always a mistake to copy a Journal.
  Journal(const Journal&) = delete;
  Journal& operator=(const Journal&) = delete;

  using SequenceNumber = JournalDelta::SequenceNumber;
  using SubscriberId = uint64_t;
  using SubscriberCallback = std::function<void()>;

  void recordCreated(RelativePathPiece fileName);
  void recordRemoved(RelativePathPiece fileName);
  void recordChanged(RelativePathPiece fileName);

  /**
   * "Renamed" means that that newName was created as a result of the mv(1).
   */
  void recordRenamed(RelativePathPiece oldName, RelativePathPiece newName);

  /**
   * "Replaced" means that that newName was overwritten by oldName as a result
   * of the mv(1).
   */
  void recordReplaced(RelativePathPiece oldName, RelativePathPiece newName);

  /**
   * Creates a journal delta that updates the hash to this new hash
   */
  void recordHashUpdate(Hash toHash);

  /**
   * Creates a journal delta that updates the hash from fromHash to toHash
   */
  void recordHashUpdate(Hash fromHash, Hash toHash);

  /**
   * Creates a journal delta that updates the hash from fromHash to toHash and
   * also sets uncleanPaths
   */
  void recordUncleanPaths(
      Hash fromHash,
      Hash toHash,
      std::unordered_set<RelativePath>&& uncleanPaths);

  /** Get a copy of the tip of the journal.
   * Will return a nullopt if the journal is empty */
  std::optional<JournalDeltaInfo> getLatest() const;

  /** Register a subscriber.
   * A subscriber is just a callback that is called whenever the
   * journal has changed.
   * It is recommended that the subscriber callback do the minimal
   * amount of work needed to schedule the real work to happen in
   * some other context because journal updates are likely to happen
   * in awkward contexts or in the middle of some batch of mutations
   * where it is not appropriate to do any heavy lifting.
   * The return value of registerSubscriber is an identifier than
   * can be passed to cancelSubscriber to later remove the registration.
   */
  SubscriberId registerSubscriber(SubscriberCallback&& callback);
  void cancelSubscriber(SubscriberId id);

  void cancelAllSubscribers();
  bool isSubscriberValid(SubscriberId id) const;

  /** Returns an option that is nullopt if the Journal is empty or an option
   * that contains valid JournalStats if the Journal is non-empty*/
  std::optional<JournalStats> getStats();

  /** Gets the sum of the modifications done by the deltas with Sequence
   * Numbers >= limitSequence, if the limitSequence is further back than the
   * Journal remembers isTruncated will be set on the JournalDeltaSum
   * The default limit value is 0 which is never assigned by the Journal
   * and thus indicates that all deltas should be summed.
   * If the limitSequence means that no deltas will match, returns nullptr.
   * */
  std::unique_ptr<JournalDeltaRange> accumulateRange(
      SequenceNumber limitSequence);
  std::unique_ptr<JournalDeltaRange> accumulateRange();

  /** Gets a vector of the modifications (newer deltas having lower indices)
   * done by the latest 'limit' deltas, if the
   * beginning of the journal is reached before 'limit' number of deltas are
   * reached then it will just return what had been currently found.
   * */
  std::vector<DebugJournalDelta> getDebugRawJournalInfo(
      SequenceNumber from,
      std::optional<size_t> limit,
      long mountGeneration) const;

  /** Removes all prior contents from the journal and sets up the journal in a
   * way such that when subscribers are notified they all get truncated results
   * */
  void flush();

  void setMemoryLimit(size_t limit);

  size_t getMemoryLimit() const;

  size_t estimateMemoryUsage() const;

 private:
  /** Add a delta to the journal and notify subscribers.
   * The delta will have a new sequence number and timestamp
   * applied.
   */
  void addDelta(FileChangeJournalDelta&& delta);
  void addDelta(HashUpdateJournalDelta&& delta, const Hash& newHash);

  static constexpr size_t kDefaultJournalMemoryLimit = 1000000000;

  struct DeltaState {
    /** The sequence number that we'll use for the next entry
     * that we link into the chain */
    SequenceNumber nextSequence{1};
    /** All recorded entries. Newer (more recent) deltas are added to the back
     * of the appropriate deque. */
    std::deque<FileChangeJournalDelta> fileChangeDeltas;
    std::deque<HashUpdateJournalDelta> hashUpdateDeltas;
    Hash currentHash = kZeroHash;
    /** The stats about this Journal up to the latest delta */
    std::optional<JournalStats> stats;
    size_t memoryLimit = kDefaultJournalMemoryLimit;
    size_t deltaMemoryUsage = 0;

    JournalDeltaPtr frontPtr() noexcept;
    void popFront();
    JournalDeltaPtr backPtr() noexcept;

    bool empty() const {
      return fileChangeDeltas.empty() && hashUpdateDeltas.empty();
    }

    bool isFileChangeInFront() const {
      bool isFileChangeEmpty = fileChangeDeltas.empty();
      bool isHashUpdateEmpty = hashUpdateDeltas.empty();
      if (!isFileChangeEmpty && !isHashUpdateEmpty) {
        return fileChangeDeltas.front().sequenceID <
            hashUpdateDeltas.front().sequenceID;
      }
      return !isFileChangeEmpty && isHashUpdateEmpty;
    }

    bool isFileChangeInBack() const {
      bool isFileChangeEmpty = fileChangeDeltas.empty();
      bool isHashUpdateEmpty = hashUpdateDeltas.empty();
      if (!isFileChangeEmpty && !isHashUpdateEmpty) {
        return fileChangeDeltas.back().sequenceID >
            hashUpdateDeltas.back().sequenceID;
      }
      return !isFileChangeEmpty && isHashUpdateEmpty;
    }

    void appendDelta(FileChangeJournalDelta&& delta) {
      fileChangeDeltas.emplace_back(std::move(delta));
    }

    void appendDelta(HashUpdateJournalDelta&& delta) {
      hashUpdateDeltas.emplace_back(std::move(delta));
    }

    JournalDelta::SequenceNumber getFrontSequenceID() const {
      if (isFileChangeInFront()) {
        return fileChangeDeltas.front().sequenceID;
      } else {
        return hashUpdateDeltas.front().sequenceID;
      }
    }
  };
  folly::Synchronized<DeltaState> deltaState_;

  /** Removes the oldest deltas until the memory usage of the journal is below
   * the journal's memory limit.
   */
  void truncateIfNecessary(DeltaState& deltaState);

  /** Tries to compact a new Journal Delta with an old one if possible,
   * returning true if it did compact it and false if not
   */
  bool compact(FileChangeJournalDelta& delta, DeltaState& deltaState);
  bool compact(HashUpdateJournalDelta& delta, DeltaState& deltaState);

  struct SubscriberState {
    SubscriberId nextSubscriberId{1};
    std::unordered_map<SubscriberId, SubscriberCallback> subscribers;
  };

  /** Add a delta to the journal without notifying subscribers.
   * The delta will have a new sequence number and timestamp
   * applied. A lock to the deltaState must be held and passed to this
   * function.
   */
  template <typename T>
  void addDeltaWithoutNotifying(T&& delta, DeltaState& deltaState);

  /** Notify subscribers that a change has happened, should be called with no
   * Journal locks held.
   */
  void notifySubscribers() const;

  size_t estimateMemoryUsage(const DeltaState& deltaState) const;

  /** Runs from the latest delta to the delta with sequence ID (if 'lengthLimit'
   * is not nullopt then checks at most 'lengthLimit' entries) and runs
   * deltaActor on each entry encountered.
   * */
  template <class FileChangeFunc, class HashUpdateFunc>
  void forEachDelta(
      const DeltaState& deltaState,
      JournalDelta::SequenceNumber from,
      std::optional<size_t> lengthLimit,
      FileChangeFunc&& fileChangeDeltaCallback,
      HashUpdateFunc&& hashUpdateDeltaCallback) const;

  folly::Synchronized<SubscriberState> subscriberState_;

  std::shared_ptr<EdenStats> edenStats_;
};
} // namespace eden
} // namespace facebook
