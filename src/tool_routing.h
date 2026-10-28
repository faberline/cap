#ifndef CAP_TOOL_ROUTING_H
#define CAP_TOOL_ROUTING_H

/* Capability rejection is not a process exit status. */
#define CAP_UNSUPPORTED 256

enum cap_tool_id { CAP_TOOL_NONE, CAP_TOOL_CAT, CAP_TOOL_WC_LINES, CAP_TOOL_WC_BYTES };

static int cap_route_eq(const char *a, const char *b) {
  while (*a && *a == *b) { a++; b++; }
  return *a == *b;
}

static const char *cap_route_base(const char *s) {
  const char *base = s;
  for (; *s; s++) if (*s == '/') base = s + 1;
  return base;
}

/* Syntax only: this function never opens a file or consumes stdin. */
static enum cap_tool_id cap_tool_probe(int count, char **words) {
  if (count < 1) return CAP_TOOL_NONE;
  const char *program = cap_route_base(words[0]);
  enum cap_tool_id tool;
  int first;
  if (cap_route_eq(program, "cat")) {
    tool = CAP_TOOL_CAT; first = 1;
  } else if (cap_route_eq(program, "wc") && count >= 2) {
    if (cap_route_eq(words[1], "-l")) tool = CAP_TOOL_WC_LINES;
    else if (cap_route_eq(words[1], "-c")) tool = CAP_TOOL_WC_BYTES;
    else return CAP_TOOL_NONE;
    first = 2;
  } else return CAP_TOOL_NONE;
  for (int idx = first; idx < count; idx++) {
    if (words[idx][0] == '-') return CAP_TOOL_NONE;
  }
  return tool;
}

#define CAP_ROUTE_RECORD(tool, backend) "{\"tool\":\"" tool "\",\"backend\":\"" backend "\",\"implementation\":\"native\",\"reason\":\"supported\"}\n"

static const char *cap_tool_record(enum cap_tool_id tool, int tiny) {
  if (tool == CAP_TOOL_CAT) return tiny ? CAP_ROUTE_RECORD("cat", "c-tiny") : CAP_ROUTE_RECORD("cat", "c-fast");
  if (tool == CAP_TOOL_WC_LINES) return CAP_ROUTE_RECORD("wc-lines", "c-fast");
  if (tool == CAP_TOOL_WC_BYTES) return CAP_ROUTE_RECORD("wc-bytes", "c-fast");
  return "";
}

#endif
