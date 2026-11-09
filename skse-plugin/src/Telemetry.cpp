#include "Telemetry.h"
#include <winsock2.h>
#include <ws2tcpip.h>
#include <Windows.h>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <mutex>
#include <thread>

namespace bridge {
namespace {
struct Store {
    std::mutex mutex;
    std::condition_variable changed;
    Snapshot state{};
    std::chrono::steady_clock::time_point publishedAt{};
    std::uint64_t revision{0};
    bool started{false};
};

Store& GetStore() {
    // SKSE does not unload plugins. Intentional process-lifetime allocation avoids
    // joining a worker or destroying a mutex while the DLL loader lock is held.
    static auto* store = new Store;
    return *store;
}

const char* HeldName(Held value) noexcept {
    switch (value) {
    case Held::Object: return "object";
    case Held::Actor: return "actor";
    default: return "none";
    }
}

void RunSender(SOCKET socket, std::uint16_t port, void (*requestRefresh)()) noexcept {
    try {
        sockaddr_in destination{};
        destination.sin_family = AF_INET;
        destination.sin_port = htons(port);
        destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        auto& store = GetStore();
        std::uint64_t sequence = 0;
        std::uint64_t sentRevision = ~std::uint64_t{0};
        auto nextHeartbeat = std::chrono::steady_clock::now();
        auto nextRefresh = nextHeartbeat;
        for (;;) {
            const auto now = std::chrono::steady_clock::now();
            if (requestRefresh && now >= nextRefresh) {
                requestRefresh(); // Only enqueues an SKSE task; no Skyrim reads here.
                nextRefresh = now + std::chrono::seconds(1);
            }
            Snapshot copy;
            {
                std::unique_lock lock(store.mutex);
                store.changed.wait_until(lock, nextHeartbeat, [&] { return store.revision != sentRevision; });
                copy = store.state;
                if (std::chrono::steady_clock::now() - store.publishedAt > std::chrono::seconds(3)) {
                    copy = {}; // A frozen game cannot hold the last resistance forever.
                }
                sentRevision = store.revision;
            }
            char packet[256]{};
            const int length = std::snprintf(packet, sizeof(packet),
                "{\"schemaVersion\":1,\"type\":\"state\",\"pid\":%lu,\"seq\":%llu,\"menuOpen\":%s,\"heldLeft\":\"%s\",\"heldRight\":\"%s\",\"gameLoaded\":%s}",
                static_cast<unsigned long>(GetCurrentProcessId()),
                static_cast<unsigned long long>(++sequence),
                copy.menuOpen ? "true" : "false", HeldName(copy.heldLeft), HeldName(copy.heldRight),
                copy.gameLoaded ? "true" : "false");
            if (length > 0 && length < static_cast<int>(sizeof(packet))) {
                // The socket is nonblocking. A missing/slow GUI or a dropped
                // packet never blocks the game; the next heartbeat repairs state.
                (void)sendto(socket, packet, length, 0,
                    reinterpret_cast<const sockaddr*>(&destination), sizeof(destination));
            }
            nextHeartbeat = std::chrono::steady_clock::now() + std::chrono::seconds(1);
        }
    } catch (...) {
        // Missing heartbeats make the external controller clear all effects.
        closesocket(socket);
    }
}
}

bool StartTelemetry(std::uint16_t port, void (*requestRefresh)()) noexcept {
    try {
        auto& store = GetStore();
        std::scoped_lock lock(store.mutex);
        if (store.started) return true;
        WSADATA data{};
        if (WSAStartup(MAKEWORD(2, 2), &data) != 0) return false;
        SOCKET socket = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
        if (socket == INVALID_SOCKET) { WSACleanup(); return false; }
        u_long nonblocking = 1;
        if (ioctlsocket(socket, FIONBIO, &nonblocking) != 0) {
            closesocket(socket); WSACleanup(); return false;
        }
        HMODULE module{};
        if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                reinterpret_cast<LPCWSTR>(&StartTelemetry), &module)) {
            closesocket(socket); WSACleanup(); return false;
        }
        try {
            std::thread(RunSender, socket, port, requestRefresh).detach();
        } catch (...) {
            closesocket(socket); WSACleanup(); return false;
        }
        store.started = true;
        return true;
    } catch (...) { return false; }
}

void Publish(const Snapshot& snapshot) noexcept {
    try {
        auto& store = GetStore();
        {
            std::scoped_lock lock(store.mutex);
            store.publishedAt = std::chrono::steady_clock::now();
            if (store.state == snapshot) return;
            store.state = snapshot;
            ++store.revision;
        }
        store.changed.notify_one();
    } catch (...) {}
}
}
