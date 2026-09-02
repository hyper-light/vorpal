// -----Type Only Export!-----//
export type { FileOption, FindConfig, NapiConfig } from './types/config'
export type { DynamicLangRegistrations } from './types/registerDynamicLang'
export type { Edit, Pos, Range } from './types/sgnode'
// Only Rule here. User can use Rule['pattern'], e.g., to get the type of subfield.
export type { Rule } from './types/rule'

// -----Runtime Value Export!-----//
export { findInFiles, kind, parse, parseAsync, parseFiles, pattern } from './types/api'
export { Lang } from './types/lang'
export { registerDynamicLanguage } from './types/registerDynamicLang'
export { SgNode, SgRoot } from './types/sgnode'
export type { BuildReport, GraphOptions, NodeInfo, ReachOptions, SearchOptions, TuneQueryInput } from './types/repo'
export {
  Index,
  indexBuild, indexBuildAsync,
  indexBuildReport, indexBuildReportAsync,
  indexSearch, indexSearchAsync,
  indexSearchRanked, indexSearchRankedAsync,
  indexGraph, indexGraphAsync,
  indexNode, indexNodeAsync,
  indexTune, indexTuneAsync,
  semanticInstall, semanticInstallAsync,
  semanticEnable, semanticEnableAsync,
  semanticDisable,
} from './types/repo'
// deprecated
export * from './types/deprecated'
