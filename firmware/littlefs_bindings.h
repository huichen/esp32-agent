// The rust bindings esp-idf-sys generates for the `joltwallet/littlefs` component.
//
// `bindings_header` in `Cargo.toml` is resolved relative to this crate, so it cannot name the
// component's own header directly - it points at this file, and this file includes it.
#pragma once

#include "esp_littlefs.h"
