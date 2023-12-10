// This file was generated by [ts-rs](https://github.com/Aleph-Alpha/ts-rs). Do not edit this file manually.
import type { FeVCToyAnatomy } from "./FeVCToyAnatomy";
import type { FeVCToyFeature } from "./FeVCToyFeature";
import type { ToyPower } from "./ToyPower";

export interface FeVCToy { toy_id: number | null, toy_name: string, toy_anatomy: FeVCToyAnatomy, toy_power: ToyPower, toy_connected: boolean, features: Array<FeVCToyFeature>, listening: boolean, osc_data: boolean, sub_id: number, }