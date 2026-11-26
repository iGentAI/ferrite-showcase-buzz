/**
 * Platforms a feature is available on.
 *
 * Kept in step with `FeaturePlatformSchema` in `manifest.ts`: the schema
 * validates the shipped manifest at RUNTIME and a value it does not know
 * discards the whole file, so a type and an enum that disagree here is not a
 * type error somewhere — it is every preview feature silently ungated.
 * `previewManifest.test.mjs` checks the shipped JSON against that enum.
 */
export type FeaturePlatform = "desktop" | "mobile" | "web";

/**
 * A single feature definition from the manifest.
 *
 * The manifest (`preview-features.json`) lists ONLY preview features —
 * membership signals "this needs gating." Anything not in the manifest is
 * treated as stable and renders unconditionally (fail-open).
 */
export interface FeatureDefinition {
  id: string;
  name: string;
  description: string;
  /** Whether the preview is enabled when the user has not chosen an override */
  defaultEnabled?: boolean;
  /** If omitted, feature is available on all platforms */
  platforms?: FeaturePlatform[];
}

/** The root manifest schema */
export interface FeaturesManifest {
  version: number;
  features: FeatureDefinition[];
}
