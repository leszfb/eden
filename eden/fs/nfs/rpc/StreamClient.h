/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once
#include <tuple>

#include <folly/SocketAddress.h>
#include <folly/String.h>
#include <folly/io/IOBufQueue.h>
#include <folly/logging/xlog.h>
#include <folly/net/NetworkSocket.h>
#include "eden/fs/nfs/xdr/XdrDeSerializer.h"
#include "eden/fs/nfs/xdr/XdrSerializer.h"

namespace facebook::eden {

class StreamClient {
  folly::IOBufQueue readBuf_;
  folly::NetworkSocket s_;
  folly::SocketAddress addr_;
  uint32_t nextXid_{1};

 public:
  explicit StreamClient(folly::SocketAddress&& addr);
  void connect();

  std::pair<std::unique_ptr<folly::IOBuf>, XdrSerializer> serializeCallHeader(
      uint32_t progNumber,
      uint32_t progVersion,
      uint32_t procNumber);

  uint32_t fillFrameAndSend(std::unique_ptr<folly::IOBuf> buf);

  template <class T>
  uint32_t serializeCall(
      uint32_t progNumber,
      uint32_t progVersion,
      uint32_t procNumber,
      const T& request) {
    auto [buf, appender] =
        serializeCallHeader(progNumber, progVersion, procNumber);
    XLOG(DBG9) << "header: " << buf->length() << " request memory size is "
               << sizeof(request);

    serializeXdr(appender, request);
    XLOG(DBG9) << "after req : " << buf->length();
    return fillFrameAndSend(std::move(buf));
  }

  std::tuple<std::unique_ptr<folly::IOBuf>, XdrDeSerializer, uint32_t>
  receiveChunk();

  template <class T>
  T receiveResult(uint32_t xid) {
    T result;
    auto [buf, deser, got_xid] = receiveChunk();
    if (xid != got_xid) {
      throw std::runtime_error("mismatched xid!");
    }
    deSerializeXdrInto(deser, result);

    if (!deser.isAtEnd()) {
      throw std::runtime_error(folly::to<std::string>(
          "unexpected trailing bytes (", deser.totalLength(), ")"));
    }

    return result;
  }

  template <class RESP, class REQ>
  RESP call(
      uint32_t progNumber,
      uint32_t progVersion,
      uint32_t procNumber,
      const REQ& request) {
    auto xid = serializeCall(progNumber, progVersion, procNumber, request);
    return receiveResult<RESP>(xid);
  }
};

} // namespace facebook::eden
