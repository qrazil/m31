/* The same program in C, with malloc/free, as the floor to compare against. */
#include <stdio.h>
#include <stdlib.h>

typedef struct Tree { struct Tree *l, *r; } Tree;

static Tree *build(int depth) {
    Tree *t = malloc(sizeof *t);
    if (depth == 0) { t->l = t->r = NULL; return t; }
    t->l = build(depth - 1);
    t->r = build(depth - 1);
    return t;
}

static long check(Tree *t) {
    if (t->l == NULL) return 1;
    return 1 + check(t->l) + check(t->r);
}

static void release(Tree *t) {
    if (t->l != NULL) { release(t->l); release(t->r); }
    free(t);
}

int main(void) {
    long total = 0;
    for (int i = 0; i < 16; i++) {
        Tree *t = build(18);
        total += check(t);
        release(t);
    }
    printf("%ld\n", total);
    return 0;
}
