/* SPDX-License-Identifier: MPL-2.0 */

#include <errno.h>
#include <inttypes.h>
#include <libubus.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define SEED UINT32_C(0x8da7213f)

static uint32_t state = SEED;

static uint32_t next_u32(void)
{
	state ^= state << 13;
	state ^= state >> 17;
	state ^= state << 5;
	return state;
}

static const char *const field_names[] = {
	"a", "ab", "abc", "abcd", "abcde", "caf\xc3\xa9", "caf\xe9",
};

static int add_value(struct blob_buf *buf, const char *name, unsigned type,
		     unsigned depth)
{
	uint32_t bits = next_u32();

	switch (type) {
	case BLOBMSG_TYPE_STRING:
	case BLOBMSG_TYPE_UNSPEC: {
		unsigned char bytes[32];
		size_t len = bits % sizeof(bytes);

		for (size_t i = 0; i < len; i++)
			bytes[i] = next_u32();
		if (type == BLOBMSG_TYPE_STRING)
			bytes[len++] = 0;
		return blobmsg_add_field(buf, type, name, bytes, len);
	}
	case BLOBMSG_TYPE_INT8:
		return blobmsg_add_u8(buf, name, bits);
	case BLOBMSG_TYPE_INT16:
		return blobmsg_add_u16(buf, name, bits);
	case BLOBMSG_TYPE_INT32:
		return blobmsg_add_u32(buf, name, bits);
	case BLOBMSG_TYPE_INT64:
		return blobmsg_add_u64(buf, name, ((uint64_t) bits << 32) | next_u32());
	case BLOBMSG_TYPE_DOUBLE: {
		uint64_t raw = ((uint64_t) bits << 32) | next_u32();
		double value;

		memcpy(&value, &raw, sizeof(value));
		return blobmsg_add_double(buf, name, value);
	}
	case BLOBMSG_TYPE_ARRAY:
	case BLOBMSG_TYPE_TABLE: {
		bool array = type == BLOBMSG_TYPE_ARRAY;
		void *cookie = blobmsg_open_nested(buf, name, array);

		if (!cookie)
			return -1;
		for (unsigned i = 0; i < bits % 5; i++) {
			unsigned child = next_u32() % (__BLOBMSG_TYPE_LAST);
			const char *key = array ? NULL : field_names[next_u32() % ARRAY_SIZE(field_names)];

			if (depth == 4 && (child == BLOBMSG_TYPE_ARRAY || child == BLOBMSG_TYPE_TABLE))
				child = BLOBMSG_TYPE_STRING;
			if (add_value(buf, key, child, depth + 1))
				return -1;
		}
		blob_nest_end(buf, cookie);
		return 0;
	}
	default:
		return -1;
	}
}

static int make_case(struct blob_buf *buf, unsigned long index)
{
	if (blobmsg_buf_init(buf))
		return -1;
	if (index == 0)
		return 0;
	if (index == 1) {
		const unsigned char text[] = { 'a', 0, 'b', 0xff, 0 };

		return blobmsg_add_u8(buf, "octet", 200) ||
		       blobmsg_add_field(buf, BLOBMSG_TYPE_STRING, "text", text, sizeof(text));
	}
	for (unsigned i = 0; i < ARRAY_SIZE(field_names); i++) {
		const char *key = field_names[next_u32() % ARRAY_SIZE(field_names)];
		unsigned type = (index + i) % (__BLOBMSG_TYPE_LAST);

		if (add_value(buf, key, type, 0))
			return -1;
	}
	return 0;
}

static int c_echo(struct ubus_context *ctx, struct ubus_object *object,
		  struct ubus_request_data *request, const char *method,
		  struct blob_attr *args)
{
	(void) object;
	(void) method;
	return ubus_send_reply(ctx, request, args);
}

static int serve(struct ubus_context *ctx)
{
	static const struct ubus_method methods[] = { UBUS_METHOD_NOARG("echo", c_echo) };
	static struct ubus_object_type type = UBUS_OBJECT_TYPE("parity.c", methods);
	struct ubus_object object = {
		.name = "parity.c",
		.type = &type,
		.methods = methods,
		.n_methods = ARRAY_SIZE(methods),
	};

	if (uloop_init() || ubus_add_object(ctx, &object))
		return EXIT_FAILURE;
	ubus_add_uloop(ctx);
	uloop_run();
	uloop_done();
	return 0;
}

struct reply {
	const struct blob_attr *args;
	bool expect_user;
	bool matches;
	unsigned count;
	size_t len;
};

static void read_reply(struct ubus_request *request, int type, struct blob_attr *data)
{
	struct reply *reply = request->priv;
	const unsigned char *bytes = blob_data(data);
	size_t expected_len = blob_len(reply->args);

	(void) type;
	reply->count++;
	reply->len = blob_len(data);
	if (reply->len < expected_len || memcmp(bytes, blob_data(reply->args), expected_len))
		return;
	if (reply->expect_user) {
		struct blob_attr *user = (struct blob_attr *) (bytes + expected_len);
		size_t remaining = reply->len - expected_len;

		reply->matches = blobmsg_check_attr_len(user, true, remaining) &&
				 blob_pad_len(user) == remaining &&
				 blobmsg_type(user) == BLOBMSG_TYPE_STRING &&
				 !strcmp(blobmsg_name(user), "user") &&
				 !strcmp(blobmsg_get_string(user), "root");
	} else {
		reply->matches = reply->len == expected_len;
	}
}

static int check_case(struct ubus_context *ctx, uint32_t id, const char *object,
		      struct blob_attr *args, unsigned long index, bool expect_user)
{
	struct reply reply = { .args = args, .expect_user = expect_user };
	int status = ubus_invoke(ctx, id, "echo", args, read_reply, &reply, 3000);

	if (!status && reply.matches && reply.count == 1)
		return 0;
	fprintf(stderr, "%s at seed %#" PRIx32 ", case %lu: status %d, %u data replies, "
		"%zu request bytes, %zu reply bytes\n", object, SEED, index, status,
		reply.count, (size_t) blob_len(args), reply.len);
	return EXIT_FAILURE;
}

static int run_cases(struct ubus_context *ctx, const char *count)
{
	uint32_t reference, rust;
	struct blob_buf buf = {};
	char *end;
	int result = EXIT_FAILURE;

	errno = 0;
	unsigned long cases = strtoul(count, &end, 10);
	if (errno || *end || !cases || count[0] == '-') {
		fprintf(stderr, "invalid case count: %s\n", count);
		return result;
	}
	if (ubus_lookup_id(ctx, "parity.c", &reference) || ubus_lookup_id(ctx, "demo", &rust)) {
		fprintf(stderr, "echo objects are unavailable\n");
		return result;
	}
	for (unsigned long i = 0; i < cases; i++) {
		if (make_case(&buf, i)) {
			fprintf(stderr, "could not build case %lu\n", i);
			goto out;
		}
		if (check_case(ctx, reference, "libubus", buf.head, i, false) ||
		    check_case(ctx, rust, "tokio-ubus", buf.head, i, true))
			goto out;
	}
	printf("%lu cases, seed %#" PRIx32 "\n", cases, SEED);
	result = 0;
out:
	blob_buf_free(&buf);
	return result;
}

int main(int argc, char **argv)
{
	if (argc != 2 && argc != 3)
		return EXIT_FAILURE;
	struct ubus_context *ctx = ubus_connect(argv[1]);

	if (!ctx) {
		fprintf(stderr, "could not connect to %s\n", argv[1]);
		return EXIT_FAILURE;
	}
	int result = argc == 2 ? serve(ctx) : run_cases(ctx, argv[2]);

	ubus_free(ctx);
	return result;
}
