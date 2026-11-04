permit(principal, action, resource)
unless { action == Action::"Write" };

permit(principal, action == Action::"Write", resource)
when { context has via && context.via != "request" };
