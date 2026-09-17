/**
 * Provider 注册表
 * 加载所有内置的 AI 平台 Provider 定义。
 * 裁剪说明：custom/ 上传机制已移除（claw 不依赖自定义 provider 注入）。
 */
const deepseek = require('./deepseek');
const claude = require('./claude');
const chatgpt = require('./chatgpt');

const builtinProviders = [deepseek, claude, chatgpt];

function getAllProviders() {
  return [...builtinProviders];
}

function getProvider(id) {
  return getAllProviders().find((p) => p.id === id) || null;
}

/** 根据 URL 自动识别所属平台 */
function getProviderByUrl(url) {
  if (!url) return null;
  return getAllProviders().find((p) => p.matchesUrl(url)) || null;
}

module.exports = {
  providers: getAllProviders(),
  getProvider,
  getAllProviders,
  getProviderByUrl,
};