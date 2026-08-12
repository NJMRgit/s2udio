/*
 * cava-node-name.c — LD_PRELOAD shim that renames cava's PipeWire node.
 *
 * Why: cava's PipeWire input hardcodes the stream name in
 * `pw_stream_new_simple(loop, "cava", props, ...)` (input/pipewire.c),
 * so every cava instance publishes `node.name = media.name = "cava"`.
 * There is no cava config option or environment variable to change it.
 *
 * This shim intercepts `pw_stream_new_simple` and injects
 * `node.name` / `media.name` from the CAVA_NODE_NAME environment variable
 * into the stream properties before the real call. s2udio spawns cava as
 *
 *   LD_PRELOAD=<this .so> CAVA_NODE_NAME=<name> cava -p <conf>
 *
 * when `node_name` is configured (cava.ron / config.ron cava section).
 * Only the s2udio-spawned cava carries the env, so other cava instances
 * on the system keep their own names. When CAVA_NODE_NAME is unset or
 * empty the shim is a transparent pass-through.
 *
 * Build (setup.sh does this):  cc -shared -fPIC -o libcavaname.so \
 *     cava-node-name.c -ldl
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>

struct pw_stream;
struct pw_loop;
struct pw_stream_events;
struct pw_properties;

typedef int (*props_set_fn)(struct pw_properties *, const char *, const char *);
typedef struct pw_stream *(*new_simple_fn)(struct pw_loop *, const char *,
                                           struct pw_properties *,
                                           const struct pw_stream_events *, void *);

struct pw_stream *pw_stream_new_simple(struct pw_loop *loop, const char *name,
                                       struct pw_properties *props,
                                       const struct pw_stream_events *events,
                                       void *data)
{
    static new_simple_fn real_new = NULL;
    static props_set_fn real_set = NULL;
    if (!real_new)
        real_new = (new_simple_fn)dlsym(RTLD_NEXT, "pw_stream_new_simple");
    if (!real_set)
        real_set = (props_set_fn)dlsym(RTLD_NEXT, "pw_properties_set");

    const char *nn = getenv("CAVA_NODE_NAME");
    if (nn && *nn && props && real_set) {
        real_set(props, "node.name", nn);
        real_set(props, "media.name", nn);
    }
    return real_new(loop, name, props, events, data);
}
