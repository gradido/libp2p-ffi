/* Two nodes in one process, through the shipped object: start, a call pinned to the other node,
 * its answer, shutdown. What it proves is the link and the C interface, not the DHT --
 * tests/network.rs covers that. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libp2p_ffi.h"

#define BUFFER_BYTES 65536

static const char *const protocols[] = {"/smoke/echo/1"};

static int start(uint8_t seed_byte, uint8_t group_byte, lp2p **node, lp2p_key key, lp2p_key group)
{
    uint8_t seed[32], group_seed[32], delegation[LP2P_DELEGATION_BYTES];
    const char *listen[] = {"/ip4/127.0.0.1/tcp/0"};
    lp2p_options opt;

    memset(seed, seed_byte, sizeof(seed));
    memset(group_seed, group_byte, sizeof(group_seed));
    if (lp2p_key_from_seed(seed, key) != LP2P_OK ||
        lp2p_key_from_seed(group_seed, group) != LP2P_OK ||
        lp2p_delegation_sign(group_seed, key, 0, delegation) != LP2P_OK)
        return -1;

    lp2p_options_default(&opt);
    memcpy(opt.node_seed, seed, sizeof(seed));
    opt.delegation = delegation;
    opt.delegation_len = sizeof(delegation);
    memcpy(opt.group, group, sizeof(lp2p_key));
    opt.listen_addrs = listen;
    opt.listen_addr_count = 1;
    opt.dht_protocol = "/smoke/kad/1";
    opt.rpc_protocols = protocols;
    opt.rpc_protocol_count = 1;
    opt.quic = 0;
    return lp2p_start(&opt, node);
}

/* Polls once and hands every record to @p visit; stops early when it answers non-zero. */
static int drain(lp2p *node, uint8_t *buf,
                 int (*visit)(lp2p *, const lp2p_event *, const uint8_t *, void *), void *ctx)
{
    int32_t n = lp2p_poll(node, buf, BUFFER_BYTES, 20);
    size_t offset = 0;

    if (n < 0)
        return n;
    while (offset + sizeof(lp2p_event) <= (size_t)n) {
        lp2p_event ev;
        int result;
        memcpy(&ev, buf + offset, sizeof(ev));
        result = visit(node, &ev, buf + offset + sizeof(ev), ctx);
        if (result != 0)
            return result;
        offset += ev.size;
    }
    return 0;
}

static int copy_address(lp2p *node, const lp2p_event *ev, const uint8_t *data, void *ctx)
{
    (void)node;
    if (ev->type != LP2P_EV_LISTENING || ev->data_len >= 256)
        return 0;
    memcpy(ctx, data, ev->data_len);
    ((char *)ctx)[ev->data_len] = '\0';
    return 1;
}

static int answer(lp2p *node, const lp2p_event *ev, const uint8_t *data, void *ctx)
{
    (void)data;
    (void)ctx;
    if (ev->type == LP2P_EV_RPC_REQUEST)
        (void)lp2p_rpc_respond(node, ev->id, (const uint8_t *)"pong", 4);
    return 0;
}

struct expect {
    uint64_t id;
    const uint8_t *node;
};

static int check(lp2p *node, const lp2p_event *ev, const uint8_t *data, void *ctx)
{
    struct expect *e = ctx;
    (void)node;
    if (ev->id != e->id)
        return 0;
    if (ev->type == LP2P_EV_RPC_FAILED) {
        fprintf(stderr, "call failed, reason %u\n", (unsigned)ev->reason);
        return -1;
    }
    if (ev->type != LP2P_EV_RPC_RESPONSE)
        return 0;
    if (ev->data_len != 4 || memcmp(data, "pong", 4) != 0 || memcmp(ev->node, e->node, 32) != 0) {
        fprintf(stderr, "unexpected answer\n");
        return -1;
    }
    return 1;
}

int main(void)
{
    static uint8_t buf_a[BUFFER_BYTES], buf_b[BUFFER_BYTES];
    lp2p *a = NULL, *b = NULL;
    lp2p_key key_a, key_b, group_a, group_b;
    char address[256] = "";
    struct expect expect;
    int round, result = 0;

    if (start(1, 0xa0, &a, key_a, group_a) != LP2P_OK ||
        start(2, 0xb0, &b, key_b, group_b) != LP2P_OK) {
        fprintf(stderr, "start failed\n");
        return 1;
    }
    for (round = 0; round < 250 && address[0] == '\0'; ++round)
        (void)drain(a, buf_a, copy_address, address);
    if (address[0] == '\0' || lp2p_add_address(b, key_a, address) != LP2P_OK) {
        fprintf(stderr, "no address\n");
        return 1;
    }

    expect.node = key_a;
    if (lp2p_rpc_request(b, group_a, key_a, 0, (const uint8_t *)"ping", 4, 5000, &expect.id) !=
        LP2P_OK) {
        fprintf(stderr, "request refused\n");
        return 1;
    }
    for (round = 0; round < 500 && result == 0; ++round) {
        (void)drain(a, buf_a, answer, NULL);
        result = drain(b, buf_b, check, &expect);
    }

    /* The topic calls, so that the shipped object is checked for their symbols too. A topic with
     * one node has no peers; what matters here is that every call is accepted. */
    {
        lp2p_key topic;
        uint64_t query = 0;
        memset(topic, 0x5a, sizeof(topic));
        if (lp2p_topic_subscribe(a, topic) != LP2P_OK ||
            lp2p_topic_publish(a, topic, (const uint8_t *)"x", 1) != LP2P_OK ||
            lp2p_topic_peers(a, topic) < 0 || lp2p_topic_unsubscribe(a, topic) != LP2P_OK ||
            lp2p_dht_provide(a, topic) != LP2P_OK || lp2p_dht_find_providers(a, topic, &query) !=
            LP2P_OK || lp2p_dht_stop_providing(a, topic) != LP2P_OK) {
            fprintf(stderr, "a topic call was refused\n");
            result = 0;
        }
    }

    (void)lp2p_shutdown(a);
    (void)lp2p_shutdown(b);
    if (result != 1) {
        fprintf(stderr, "no answer\n");
        return 1;
    }
    printf("libp2p-ffi smoke: pong from %s, abi %u\n", address, (unsigned)lp2p_abi_version());
    return 0;
}
