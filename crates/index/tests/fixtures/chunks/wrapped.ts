import { fetchJson } from "./net";

export function loadUser(id: string) {
  return fetchJson(`/users/${id}`);
}

export default function loadAll() {
  return fetchJson("/users");
}

(function bootstrap() {
  function inner() {
    return fetchJson("/boot");
  }
  inner();
})();

describe("users", () => {
  it("loads", async () => {
    await fetchJson("/users/1");
  });
});

module.exports = {
  helper() {
    return fetchJson("/helper");
  },
};

if (process.env.DEBUG) {
  function debugLoad() {
    return fetchJson("/debug");
  }
  debugLoad();
}

const later = () => fetchJson("/later");

let seed = fetchJson
function skipMe() { return 1; }
(function () { fetchJson("/asi"); })();
