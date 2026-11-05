// C interface to sp404-drive's core (ffi/src/lib.rs), for the macOS FSKit extension.
// Keep in step with ffi/src/lib.rs.
//
// Calls return 0 or an errno value; sp_read and sp_write return a byte count, or a
// negative errno. Item 2 is the root, holding the folders "Internal" and "SD-Card".

#ifndef SP404_H
#define SP404_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SP404_ROOT 2

typedef struct SpDrive SpDrive;

typedef struct {
    uint64_t id;
    uint64_t size;
    bool is_dir;
} SpAttr;

// Log lines from the core: level 1 error, 2 warn, 3 info, 4 debug, 5 trace.
typedef void (*SpLogFn)(int32_t level, const char *message);
void sp_set_logger(SpLogFn f, int32_t max_level);

// Called once, from another thread, if the unit is unplugged or switched off.
typedef void (*SpClosedFn)(void *context);

// Connect to the unit (port NULL: find it) and open the drive. NULL on failure, with the
// reason in err.
SpDrive *sp_drive_open(const char *port, bool read_only, uint32_t cache_secs,
                       SpClosedFn on_closed, void *context, char *err, size_t err_len);
// Save everything, wait out the unit's session (up to 3 s), close the port, free.
void sp_drive_close(SpDrive *d);
int64_t sp_drive_time(const SpDrive *d);
bool sp_drive_read_only(const SpDrive *d);
void sp_space(const SpDrive *d, uint64_t *total, uint64_t *free);

int32_t sp_lookup(const SpDrive *d, uint64_t dir, const char *name, SpAttr *out,
                  char *name_out, size_t name_cap);
int32_t sp_getattr(const SpDrive *d, uint64_t id, SpAttr *out);

// Return true to stop the listing.
typedef bool (*SpListFn)(void *context, const char *name, const SpAttr *attr);
int32_t sp_list(const SpDrive *d, uint64_t dir, SpListFn f, void *context);

void sp_reclaim(const SpDrive *d, uint64_t id);
int32_t sp_open(const SpDrive *d, uint64_t id, bool write);
int32_t sp_close(const SpDrive *d, uint64_t id, bool keep_read, bool keep_write);
int64_t sp_read(const SpDrive *d, uint64_t id, uint64_t offset, uint8_t *buf, size_t len);
int64_t sp_write(const SpDrive *d, uint64_t id, uint64_t offset, const uint8_t *data,
                 size_t len);
int32_t sp_set_size(const SpDrive *d, uint64_t id, uint64_t size);

int32_t sp_create(const SpDrive *d, uint64_t dir, const char *name, bool is_dir,
                  SpAttr *out);
int32_t sp_remove(const SpDrive *d, uint64_t dir, const char *name);
int32_t sp_rename(const SpDrive *d, uint64_t from_dir, const char *from_name,
                  uint64_t to_dir, const char *to_name, bool replace, char *name_out,
                  size_t name_cap);
int32_t sp_sync(const SpDrive *d);

#ifdef __cplusplus
}
#endif

#endif
