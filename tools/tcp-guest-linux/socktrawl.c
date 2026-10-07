// SPDX-License-Identifier: GPL-2.0-only
/* Nonblocking, destructive batch reads of SVSM TCP observations. */
#include <linux/fs.h>
#include <linux/miscdevice.h>
#include <linux/mm.h>
#include <linux/module.h>
#include <linux/uaccess.h>
#include <asm/sev.h>

#define FRAME_SIZE 56

static int socktrawl_open(struct inode *inode, struct file *file)
{
	unsigned long page = get_zeroed_page(GFP_KERNEL);

	if (!page)
		return -ENOMEM;
	file->private_data = (void *)page;
	return nonseekable_open(inode, file);
}

static ssize_t socktrawl_read(struct file *file, char __user *output,
			     size_t count, loff_t *position)
{
	int length;

	if (!count)
		return 0;
	count = min_t(size_t, count, PAGE_SIZE);
	count -= count % FRAME_SIZE;
	if (!count)
		return -EINVAL;
	length = snp_socktrawl_read(file->private_data, count);
	if (length < 0)
		return length;
	if (!length)
		return -EAGAIN;
	if (copy_to_user(output, file->private_data, length))
		return -EFAULT;
	return length;
}

static int socktrawl_release(struct inode *inode, struct file *file)
{
	free_page((unsigned long)file->private_data);
	return 0;
}

static const struct file_operations socktrawl_fops = {
	.owner = THIS_MODULE,
	.open = socktrawl_open,
	.read = socktrawl_read,
	.release = socktrawl_release,
	.llseek = no_llseek,
};

static struct miscdevice socktrawl_device = {
	.minor = MISC_DYNAMIC_MINOR,
	.name = "socktrawl",
	.fops = &socktrawl_fops,
	.mode = 0400,
};

module_misc_device(socktrawl_device);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Read SVSM socket observations through /dev/socktrawl");
