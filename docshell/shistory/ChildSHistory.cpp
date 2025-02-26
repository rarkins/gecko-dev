/* -*- Mode: C++; tab-width: 8; indent-tabs-mode: nil; c-basic-offset: 2 -*- */
/* vim: set ts=8 sts=2 et sw=2 tw=80: */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "mozilla/dom/ChildSHistory.h"
#include "mozilla/dom/ChildSHistoryBinding.h"
#include "mozilla/dom/CanonicalBrowsingContext.h"
#include "mozilla/dom/ContentChild.h"
#include "mozilla/dom/ContentFrameMessageManager.h"
#include "nsIXULRuntime.h"
#include "nsComponentManagerUtils.h"
#include "nsSHEntry.h"
#include "nsSHistory.h"
#include "nsDocShell.h"
#include "nsXULAppAPI.h"

extern mozilla::LazyLogModule gSHLog;

namespace mozilla {
namespace dom {

ChildSHistory::ChildSHistory(BrowsingContext* aBrowsingContext)
    : mBrowsingContext(aBrowsingContext) {}

void ChildSHistory::SetBrowsingContext(BrowsingContext* aBrowsingContext) {
  mBrowsingContext = aBrowsingContext;
}

void ChildSHistory::SetIsInProcess(bool aIsInProcess) {
  if (!aIsInProcess) {
    mHistory = nullptr;

    return;
  }

  if (mHistory || mozilla::SessionHistoryInParent()) {
    return;
  }

  mHistory = new nsSHistory(mBrowsingContext);
}

int32_t ChildSHistory::Count() {
  if (mozilla::SessionHistoryInParent() || mAsyncHistoryLength) {
    uint32_t length = mLength;
    for (uint32_t i = 0; i < mPendingSHistoryChanges.Length(); ++i) {
      length += mPendingSHistoryChanges[i].mLengthDelta;
    }

    if (mAsyncHistoryLength) {
      MOZ_ASSERT(!mozilla::SessionHistoryInParent());
      // XXX The assertion may be too strong here, but it fires only
      //    when the pref is enabled.
      MOZ_ASSERT(mHistory->GetCount() == int32_t(length));
    }
    return length;
  }
  return mHistory->GetCount();
}

int32_t ChildSHistory::Index() {
  if (mozilla::SessionHistoryInParent() || mAsyncHistoryLength) {
    uint32_t index = mIndex;
    for (uint32_t i = 0; i < mPendingSHistoryChanges.Length(); ++i) {
      index += mPendingSHistoryChanges[i].mIndexDelta;
    }

    if (mAsyncHistoryLength) {
      MOZ_ASSERT(!mozilla::SessionHistoryInParent());
      int32_t realIndex;
      mHistory->GetIndex(&realIndex);
      // XXX The assertion may be too strong here, but it fires only
      //    when the pref is enabled.
      MOZ_ASSERT(realIndex == int32_t(index));
    }
    return index;
  }
  int32_t index;
  mHistory->GetIndex(&index);
  return index;
}

nsID ChildSHistory::AddPendingHistoryChange() {
  int32_t indexDelta = 1;
  int32_t lengthDelta = (Index() + indexDelta) - (Count() - 1);
  return AddPendingHistoryChange(indexDelta, lengthDelta);
}

nsID ChildSHistory::AddPendingHistoryChange(int32_t aIndexDelta,
                                            int32_t aLengthDelta) {
  nsID changeID = {};
  nsContentUtils::GenerateUUIDInPlace(changeID);
  PendingSHistoryChange change = {changeID, aIndexDelta, aLengthDelta};
  mPendingSHistoryChanges.AppendElement(change);
  return changeID;
}

void ChildSHistory::SetIndexAndLength(uint32_t aIndex, uint32_t aLength,
                                      const nsID& aChangeID) {
  mIndex = aIndex;
  mLength = aLength;
  mPendingSHistoryChanges.RemoveElementsBy(
      [aChangeID](const PendingSHistoryChange& aChange) {
        return aChange.mChangeID == aChangeID;
      });
}

void ChildSHistory::Reload(uint32_t aReloadFlags, ErrorResult& aRv) {
  if (mozilla::SessionHistoryInParent()) {
    if (XRE_IsParentProcess()) {
      nsISHistory* shistory =
          mBrowsingContext->Canonical()->GetSessionHistory();
      if (shistory) {
        aRv = shistory->Reload(aReloadFlags);
      }
    } else {
      ContentChild::GetSingleton()->SendHistoryReload(mBrowsingContext,
                                                      aReloadFlags);
    }

    return;
  }
  aRv = mHistory->Reload(aReloadFlags);
}

bool ChildSHistory::CanGo(int32_t aOffset) {
  CheckedInt<int32_t> index = Index();
  index += aOffset;
  if (!index.isValid()) {
    return false;
  }
  return index.value() < Count() && index.value() >= 0;
}

void ChildSHistory::Go(int32_t aOffset, bool aRequireUserInteraction,
                       ErrorResult& aRv) {
  CheckedInt<int32_t> index = Index();
  MOZ_LOG(
      gSHLog, LogLevel::Debug,
      ("ChildSHistory::Go(%d), current index = %d", aOffset, index.value()));
  if (aRequireUserInteraction && aOffset != -1 && aOffset != 1) {
    NS_ERROR(
        "aRequireUserInteraction may only be used with an offset of -1 or 1");
    aRv.Throw(NS_ERROR_INVALID_ARG);
    return;
  }

  while (true) {
    index += aOffset;
    if (!index.isValid()) {
      aRv.Throw(NS_ERROR_FAILURE);
      return;
    }

    // See Bug 1650095.
    if (mozilla::SessionHistoryInParent() && !mPendingEpoch) {
      mPendingEpoch = true;
      RefPtr<ChildSHistory> self(this);
      NS_DispatchToCurrentThread(
          NS_NewRunnableFunction("UpdateEpochRunnable", [self] {
            self->mHistoryEpoch++;
            self->mPendingEpoch = false;
          }));
    }

    // Check for user interaction if desired, except for the first and last
    // history entries. We compare with >= to account for the case where
    // aOffset >= Count().
    if (!aRequireUserInteraction || index.value() >= Count() - 1 ||
        index.value() <= 0) {
      break;
    }
    if (mHistory && mHistory->HasUserInteractionAtIndex(index.value())) {
      break;
    }
  }

  GotoIndex(index.value(), aOffset, aRv);
}

void ChildSHistory::AsyncGo(int32_t aOffset, bool aRequireUserInteraction,
                            CallerType aCallerType, ErrorResult& aRv) {
  CheckedInt<int32_t> index = Index();
  MOZ_LOG(gSHLog, LogLevel::Debug,
          ("ChildSHistory::AsyncGo(%d), current index = %d", aOffset,
           index.value()));
  nsresult rv = mBrowsingContext->CheckLocationChangeRateLimit(aCallerType);
  if (NS_FAILED(rv)) {
    MOZ_LOG(gSHLog, LogLevel::Debug, ("Rejected"));
    aRv.Throw(rv);
    return;
  }

  RefPtr<PendingAsyncHistoryNavigation> asyncNav =
      new PendingAsyncHistoryNavigation(this, aOffset, aRequireUserInteraction);
  mPendingNavigations.insertBack(asyncNav);
  NS_DispatchToCurrentThread(asyncNav.forget());
}

void ChildSHistory::GotoIndex(int32_t aIndex, int32_t aOffset,
                              ErrorResult& aRv) {
  MOZ_LOG(gSHLog, LogLevel::Debug,
          ("ChildSHistory::GotoIndex(%d, %d), epoch %" PRIu64, aIndex, aOffset,
           mHistoryEpoch));
  if (mozilla::SessionHistoryInParent()) {
    nsCOMPtr<nsISHistory> shistory = mHistory;
    mBrowsingContext->HistoryGo(
        aOffset, mHistoryEpoch, [shistory](int32_t&& aRequestedIndex) {
          // FIXME Should probably only do this for non-fission.
          if (shistory) {
            shistory->InternalSetRequestedIndex(aRequestedIndex);
          }
        });
  } else {
    aRv = mHistory->GotoIndex(aIndex);
  }
}

void ChildSHistory::RemovePendingHistoryNavigations() {
  // Per the spec, this generally shouldn't remove all navigations - it
  // depends if they're in the same document family or not.  We don't do
  // that.  Also with SessionHistoryInParent, this can only abort AsyncGo's
  // that have not yet been sent to the parent - see discussion of point
  // 2.2 in comments in nsDocShell::UpdateURLAndHistory()
  MOZ_LOG(gSHLog, LogLevel::Debug,
          ("ChildSHistory::RemovePendingHistoryNavigations: %zu",
           mPendingNavigations.length()));
  mPendingNavigations.clear();
}

void ChildSHistory::EvictLocalContentViewers() {
  if (!mozilla::SessionHistoryInParent()) {
    mHistory->EvictAllContentViewers();
  }
}

nsISHistory* ChildSHistory::GetLegacySHistory(ErrorResult& aError) {
  if (mozilla::SessionHistoryInParent()) {
    aError.ThrowTypeError(
        "legacySHistory is not available with session history in the parent.");
    return nullptr;
  }

  MOZ_RELEASE_ASSERT(mHistory);
  return mHistory;
}

nsISHistory* ChildSHistory::LegacySHistory() {
  IgnoredErrorResult ignore;
  nsISHistory* shistory = GetLegacySHistory(ignore);
  MOZ_RELEASE_ASSERT(shistory);
  return shistory;
}

NS_INTERFACE_MAP_BEGIN_CYCLE_COLLECTION(ChildSHistory)
  NS_WRAPPERCACHE_INTERFACE_MAP_ENTRY
  NS_INTERFACE_MAP_ENTRY(nsISupports)
NS_INTERFACE_MAP_END

NS_IMPL_CYCLE_COLLECTING_ADDREF(ChildSHistory)
NS_IMPL_CYCLE_COLLECTING_RELEASE(ChildSHistory)

NS_IMPL_CYCLE_COLLECTION_WRAPPERCACHE(ChildSHistory, mBrowsingContext, mHistory)

JSObject* ChildSHistory::WrapObject(JSContext* cx,
                                    JS::Handle<JSObject*> aGivenProto) {
  return ChildSHistory_Binding::Wrap(cx, this, aGivenProto);
}

nsISupports* ChildSHistory::GetParentObject() const {
  return xpc::NativeGlobal(xpc::PrivilegedJunkScope());
}

void ChildSHistory::SetAsyncHistoryLength(bool aEnable, ErrorResult& aRv) {
  if (mozilla::SessionHistoryInParent() || !mHistory) {
    aRv.Throw(NS_ERROR_FAILURE);
    return;
  }

  if (mAsyncHistoryLength == aEnable) {
    return;
  }

  mAsyncHistoryLength = aEnable;
  if (mAsyncHistoryLength) {
    mHistory->GetIndex(&mIndex);
    mLength = mHistory->GetCount();
  } else {
    mIndex = -1;
    mLength = 0;
    mPendingSHistoryChanges.Clear();
  }
}

}  // namespace dom
}  // namespace mozilla
