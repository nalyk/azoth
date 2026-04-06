// AZOTH preload — injects build-time globals and compatibility shims

// 1. MACRO global (replaces bun:bundle compile-time injection)
(globalThis as any).MACRO = {
  VERSION: '1.0.25-azoth',
  BUILD_DATE: new Date().toISOString(),
};

// 2. require() shim for ESM context
// Claude Code uses require() for lazy circular-dep breaking. Bun's native
// require() throws on async ESM modules. This shim uses Bun's synchronous
// import.meta.require which handles ESM modules correctly.
import { createRequire } from 'module';
const _require = createRequire(import.meta.url);
(globalThis as any).require = (id: string) => {
  try {
    return _require(id);
  } catch {
    // Fallback: return empty module for non-critical lazy imports
    return new Proxy({}, {
      get: (_, prop) => {
        if (prop === '__esModule') return true;
        if (prop === 'default') return {};
        return () => null;
      }
    });
  }
};
