import { invoke } from "@tauri-apps/api";
import type { ChangeEvent } from "react";
import { useEffect, useState } from "react";
import type { FeVCFeatureType } from "../../src-tauri/bindings/FeVCFeatureType";
import { DEBOUNCE_TIME, SIMULATE_TOY_FEATURE } from "../data/constants";

export default function useSimulate(
  toyId: number,
  featureIndex: number,
  featureType: FeVCFeatureType
) {
  const [simulate, setSimulate] = useState(false);
  const [simulateLevel, setSimulateLevel] = useState(0.5);

  function simulateHandler(e: ChangeEvent<HTMLInputElement>) {
    setSimulate(e.target.checked);
  }

  function simulateLevelHandler(e: number) {
    setSimulateLevel(e);
  }

  useEffect(() => {
    if (simulate) {
      const t = setTimeout(() => {
        invokeSimulation(simulateLevel);
      }, DEBOUNCE_TIME);
      return () => clearTimeout(t);
    } else {
      invokeSimulation(0);
    }
  }, [simulate, simulateLevel]);

  async function invokeSimulation(floatLevel: number) {
    await invoke(SIMULATE_TOY_FEATURE, {
      toyId,
      featureIndex,
      featureType,
      floatLevel,
    });
  }

  return { simulate, simulateHandler, simulateLevel, simulateLevelHandler };
}