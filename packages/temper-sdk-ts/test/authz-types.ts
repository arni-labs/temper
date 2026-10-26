// Compile with strict null checks to pin the authorize wire contract.
import type { AuthzResponse } from "../src/client.js";

const allowed: AuthzResponse = { allowed: true, reason: null };

function reasonText(answer: AuthzResponse): string {
  if (answer.reason !== undefined) {
    // @ts-expect-error An undefined-only guard must not make a nullable reason safe.
    answer.reason.trim();
  }
  return answer.reason == null ? "" : answer.reason.trim();
}

void allowed;
void reasonText;
