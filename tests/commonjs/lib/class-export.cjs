module.exports = class Point {
    constructor(x, y) { this.x = x; this.y = y; }
    len() { return Math.hypot(this.x, this.y); }
};
