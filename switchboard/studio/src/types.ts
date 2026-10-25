// 06 篇 manifest 的 TS 镜像（spec/06 §2）
export type Stage = 'discovery' | 'punch' | 'relay' | 'transport' | 'media' | 'codec';

export type Compliance = 'L-A' | 'L-B' | 'L-C';

export interface ModuleManifest {
  id: string;
  version: string;
  stage: Stage;
  binding: 'native-rust' | 'wasm' | 'process';
  provides?: Record<string, unknown>;
  telemetry: string;
  compliance: Compliance;
  bench_note?: string;
  origin?: { adapted_from: string; upstream_ref?: string[] };
  principles_card?: string | null;
  license?: string;
}

export interface Registry {
  spec: string;
  modules: ModuleManifest[];
}
