#include <linux/slab.h>

#define ALLOC(n) kmalloc((n), GFP_KERNEL)

struct probe {
	int size;
	void *buf;
};

#ifdef CONFIG_PROBE_FAST
static void *fast_path(struct probe *p)
{
	p->buf = kmalloc(p->size, GFP_ATOMIC);
	return p->buf;
}
#else
static void *fast_path(struct probe *p)
{
	p->buf = kmalloc(p->size, GFP_KERNEL);
	if (!p->buf)
		return NULL;
	return p->buf;
}
#endif

int probe_init(struct probe *p, int n)
{
	p->size = n;
	if (!fast_path(p))
		return -ENOMEM;
	return 0;
}
EXPORT_SYMBOL(probe_init);

#if IS_ENABLED(CONFIG_PROBE_DEBUG)
void probe_dump(struct probe *p)
{
	void *scratch = ALLOC(p->size);
	pr_info("probe %d\n", p->size);
	kfree(scratch);
}
#endif

static struct probe *probe_of(struct list_head *l)
{
	struct probe *p = container_of(l, struct probe, list);
	kfree(p, );
	list_for_each_entry(p, l, list)
		(void) probe_init(p, 1);
	return p;
}

static int __init probe_module_init(void)
{
	struct probe *p = kmalloc(sizeof(*p), GFP_KERNEL);
	if (!p)
		return -ENOMEM;
	return probe_init(p, 16);
}
module_init(probe_module_init);
