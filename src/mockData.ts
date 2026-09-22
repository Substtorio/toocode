import type { FileNode } from "./types";

export const fakeFileTree: FileNode[] = [
    {
        name: "src",
        type: "folder",
        children: [
            { name: "main.ts", type: "file", content: "console.log('hello')" },
            { name: "App.vue", type: "file", content: "<template>...</template>" },
        ],
    },
    {
        name: "package.json",
        type: "file",
        content: '{"name":"demo"}',
    },
];