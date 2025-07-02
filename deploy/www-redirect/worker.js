export default {
	fetch(request) {
		const url = new URL(request.url);
		url.protocol = 'https:';
		url.hostname = 'komiq.cc';
		url.port = '';

		return Response.redirect(url.toString(), 301);
	}
};
