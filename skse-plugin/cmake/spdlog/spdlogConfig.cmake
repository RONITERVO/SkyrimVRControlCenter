# spdlog is a pinned, already-configured sibling target; no external package lookup.
if(NOT TARGET spdlog::spdlog)
  message(FATAL_ERROR "The pinned spdlog target must be configured first")
endif()
set(spdlog_FOUND TRUE)
