import { helper } from "./util";
export function top(a: number): number {
  function inner() {}
  return a;
}
function* gen() {}
declare function ambient(): void;
export function over(a: string): void;
export function over(a: any) {}
export class Point<T> extends Base implements Shape {
  x: number = 1;
  onClick = () => this.x;
  constructor() {
    super();
  }
  get len(): number {
    return 0;
  }
  move(dx: number): void;
  move(dx: any) {}
  #secret() {}
}
export abstract class Shape2 {
  abstract area(): number;
  describe() {}
}
export interface Shape {
  area(): number;
}
type Id = string | number;
export enum Color { Red, Green = 2 }
namespace Geo {
  export function dist() {}
}
declare module "ext" {
  export function e(): void;
}
export const LIMIT = 10, OTHER = 2;
export const handler = async (x: number) => x;
const fn = function named() {};
let counter = 0;
declare const amb: number;
if (counter) {
  const hidden = 1;
}
describe("x", () => {
  function cb() {}
});
const obj = { method() {}, arrow: () => 1 };
export { helper, format as fmt } from "./util";
export * from "./all";
export default function main() {}
@Component({ selector: "w" })
export class Widget {
  @Input() name: string;
  @HostListener("click") onClick() {}
}
