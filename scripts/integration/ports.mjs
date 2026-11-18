import { availablePorts } from "../../packages/runtime/src/testing.ts";

export const PORT_OFFSETS = { core: 0, discord: 1, runtime: 2, "runtime-t3": 3, web: 4 };

export const availableBasePort = () => availablePorts(Object.keys(PORT_OFFSETS).length);
