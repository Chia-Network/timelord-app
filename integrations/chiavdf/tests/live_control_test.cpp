#include "../timelord_live_control.hpp"
#include "../timelord_telemetry.hpp"
#include <cassert>
#include <vector>
using namespace timelord_control;
int main() {
    // Sparse high-clock PLL entries must not be advertised as exact 10 MHz steps.
    struct Entry { double freq; };
    const Entry pll[]={{1490.},{1500.},{1511.111111},{1520.},{1525.},{1533.333333},{1540.},{1560.},{1566.666667},{1571.428571},{1580.},{1600.}};
    assert(pll[frequency_index(1510.,pll,12)].freq==1500.);
    for(double f:{1510.,1530.,1570.,1590.}) assert(!supported_frequency(f,pll,12));
    for(double f:{1500.,1520.,1540.,1560.,1580.,1600.}) assert(supported_frequency(f,pll,12));
    const Entry upper_pll[]={{1600.},{1620.},{2200.}};
    assert(supported_frequency(1620.,upper_pll,3));
    assert(supported_frequency(2200.,upper_pll,3));
    assert(!supported_frequency(2190.,upper_pll,3));
    // An idle infused-chain board must keep reporting temperature even when
    // its iteration counter never reaches the usual active polling interval.
    using Poll = timelord_telemetry::Poll;
    const auto start = Poll::Clock::time_point{};
    Poll telemetry(start);
    int samples = 0;
    for (int second = 0; second <= 180; ++second) {
        if (telemetry.due(false, start + std::chrono::seconds(second))) ++samples;
        assert(samples == second / 5);
    }
    assert(telemetry.due(true, start + std::chrono::seconds(181)));
    assert(!telemetry.due(false, start + std::chrono::seconds(185)));
    assert(telemetry.due(false, start + std::chrono::seconds(186)));
    timelord_telemetry::Speed speed;
    assert(!speed.update(false,0,0));
    assert(*speed.update(true,100,100)==-1.);
    assert(!speed.update(true,500100,500100));
    assert(*speed.update(true,1000100,1000100)==1000000.);
    assert(*speed.update(true,500,2000100)==-1.); // recovery rollback
    assert(*speed.update(true,600,100)==-1.); // new challenge
    assert(*speed.update(false,0,0)==-1.);
    assert(!speed.update(false,0,0));
    Point current{1450.,.95};std::vector<std::string> actions;
    auto read=[&](){return current;};
    auto voltage=[&](double v){actions.push_back("voltage");current.voltage=v;};
    auto frequency=[&](double f){actions.push_back("frequency");current.frequency=f;};
    auto pause=[&](bool v){actions.push_back(v?"resume":"pause");};
    apply(Point{1460.,1.},read,voltage,frequency,pause);
    assert((actions==std::vector<std::string>{"pause","voltage","frequency","resume"}));
    actions.clear();apply(Point{1440.,.95},read,voltage,frequency,pause);
    assert((actions==std::vector<std::string>{"pause","frequency","voltage","resume"}));
    actions.clear();
    try { apply(Point{2201.,1.},read,voltage,frequency,pause);assert(false); } catch(...){}
    assert(actions.empty());
    try {apply(Point{1450.,1.},read,[](double){throw std::runtime_error("failed");},frequency,pause);assert(false);}catch(...){}
    assert((actions==std::vector<std::string>{"pause"}));
    // Incorrect readback must never resume the engines.
    actions.clear();current=Point{1450.,.95};int reads=0;
    try {apply(Point{1460.,1.},[&](){return ++reads==1?current:Point{1000.,1.};},voltage,frequency,pause);assert(false);}catch(...){}
    assert(actions.back()!="resume");
    auto exchange=[](std::string request, bool accepted) {
        int in[2],out[2];assert(!pipe(in)&&!pipe(out));Channel channel(in[0],out[1]);assert(channel.start());
        int writes=0;assert(write(in[1],request.data(),request.size())==static_cast<ssize_t>(request.size()));
        assert(channel.poll([&](Point p){++writes;return p;})==accepted);
        assert(writes==(accepted?1:0));
        for(int fd:{in[0],in[1],out[0],out[1]})close(fd);
    };
    exchange("TLSET 1 1 1450 1\n",true);
    exchange("TLSET 1 1 2200 1\n",true);
    assert(valid(Point{2200.,1.}));
    assert(!valid(Point{2200.,1.01}));
    for(auto bad:{"TLSET 1 -1 1450 1\n","TLSET 1 1 1450 1.1\n","TLSET 1 1 nan 1\n","TLSET 1 1 2201 1\n","TLSET 1 1 1450 1 extra\n","TLSET 1 18446744073709551616 1450 1\n"})exchange(bad,false);
    int in[2],out[2];assert(!pipe(in)&&!pipe(out));Channel channel(in[0],out[1]);assert(channel.start());int writes=0;
    auto apply=[&](Point p){++writes;return p;};
    assert(write(in[1],"TLSET 1 1 1450",14)==14);assert(channel.poll(apply));assert(writes==0);
    assert(write(in[1]," 1\n",3)==3);assert(channel.poll(apply));assert(writes==1);
    const char* duplicate="TLSET 1 1 1450 1\n";assert(write(in[1],duplicate,strlen(duplicate))>0);assert(!channel.poll(apply));assert(writes==1);
    for(int fd:{in[0],in[1],out[0],out[1]})close(fd);
    assert(!pipe(in)&&!pipe(out));Channel lost(in[0],out[1]);assert(lost.start());close(out[0]);
    const char* request="TLSET 1 1 1450 1\n";assert(write(in[1],request,strlen(request))>0);
    assert(!lost.poll([](Point p){return p;}));
    for(int fd:{in[0],in[1],out[1]})close(fd);
    // Preserve the native failure while still rejecting the command and stopping.
    assert(!pipe(in)&&!pipe(out));Channel rejected(in[0],out[1]);assert(rejected.start());
    assert(write(in[1],request,strlen(request))>0);
    assert(!rejected.poll([](Point)->Point{throw std::runtime_error("pll_write");}));
    char reason[128]{};const auto size=::read(out[0],reason,sizeof(reason)-1);assert(size>0);
    assert(std::string(reason,static_cast<size_t>(size))=="TLERR 1 1 pll_write\n");
    for(int fd:{in[0],in[1],out[0],out[1]})close(fd);
    puts("native live-control fixtures passed (no USB)");
}
