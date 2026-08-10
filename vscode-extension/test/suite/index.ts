// 测试套件入口
//
// vscode-test 要求导出一个 activate 函数，其中注册 mocha 测试。
// 手动列举测试文件，避免引入 glob 依赖。

import * as path from 'path';
import Mocha from 'mocha';

export function activate(): Promise<void> {
    const mocha = new Mocha({
        ui: 'tdd',
        color: true,
        timeout: 10000,
    });

    // 手动列举测试文件（编译后从 out/test/suite/ 运行）
    const tests = ['./acp-transport.test', './handlers.test', './setup-wizard.test'];
    for (const t of tests) {
        mocha.addFile(path.resolve(__dirname, `${t}.js`));
    }

    return new Promise((resolve, reject) => {
        try {
            mocha.run((failures: number) => {
                if (failures > 0) {
                    reject(new Error(`${failures} tests failed`));
                } else {
                    resolve();
                }
            });
        } catch (e) {
            reject(e);
        }
    });
}
