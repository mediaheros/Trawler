import { describe, expect, it } from "vitest";

import { detectDesktopPlatform } from "./platform";

describe("desktop platform detection", () => {
  it("distinguishes macOS, Windows, and Linux", () => {
    expect(detectDesktopPlatform("MacIntel", "Mozilla/5.0")).toBe("macos");
    expect(detectDesktopPlatform("Win32", "Mozilla/5.0 Windows NT 10.0")).toBe("windows");
    expect(detectDesktopPlatform("Linux x86_64", "Mozilla/5.0 X11; Linux x86_64")).toBe("linux");
  });
});
